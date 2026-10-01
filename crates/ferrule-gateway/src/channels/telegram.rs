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
//!
//! A bot that stops answering is the worst failure there is: the owner can't
//! see why from the chat. So every request has a deadline (a half-open
//! connection can't park the long poll forever while the process looks
//! healthy), and a failed poll is retried with backoff rather than ending the
//! adapter. Only a rejected token stops it — retrying can't fix that.
//!
//! And every reason it doesn't answer is said out loud (M19c): a 409
//! Conflict that lasts is told to the owner, a webhook is removed and the
//! owner told why, a message with no text gets a plain reply, and a chat
//! that isn't allowed is a warning (once an hour) that `/status` shows.

use crate::channel::{Button, ButtonAction, Channel, ChannelCapabilities};
use crate::channels::files::{self, Inbox, Refused, Saved};
use crate::error::GatewayError;
use crate::health::human;
use crate::message::{Attachment, InboundMessage, OutboundMessage};
use crate::stream::chunks;
use crate::typing::Typing;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::mpsc;

/// How long Telegram may hold a `getUpdates` call open when there's nothing new.
const LONG_POLL_SECS: u64 = 30;
/// The long poll plus room for a slow answer; past this the connection is dead.
const POLL_DEADLINE: Duration = Duration::from_secs(LONG_POLL_SECS + 15);
/// Every other call (send, edit) and the connect itself.
const REQUEST_DEADLINE: Duration = Duration::from_secs(30);
const CONNECT_DEADLINE: Duration = Duration::from_secs(10);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// How long 409s go on before the owner is told, and how long polling has
/// to stay clean after the last one before it counts as over.
pub const CONFLICT_AFTER: Duration = Duration::from_secs(60);
/// A chat that isn't allowed is logged at most this often.
const IGNORED_WARN_EVERY: Duration = Duration::from_secs(3600);
/// The most a `sendMessage` text may hold, in UTF-16 units as Telegram
/// counts (its documented limit; Discord has 2000, Slack 4000).
pub const MESSAGE_LIMIT: usize = 4096;

/// Why a poll failed: `Retry` heals by itself (network, Telegram's own 5xx,
/// a rate limit, a bad body); `Conflict` is a 409, which needs the owner if
/// it lasts; `Fatal` never heals (the token was rejected).
enum PollError {
    Retry(String, Option<Duration>),
    Conflict(String),
    Fatal(String),
}

/// A run of 409s from `getUpdates`. Two pollers on one token take turns,
/// so a good poll in between doesn't end it; a clean `CONFLICT_AFTER` does.
struct Conflict {
    since: Instant,
    last: Instant,
    /// Telegram's description of the last 409.
    description: String,
    told: bool,
}

/// A message the adapter understood, and what it couldn't read in it.
struct Parsed {
    msg: InboundMessage,
    /// The kind of attachment that wasn't read ("photo", "voice message").
    unread: Option<&'static str>,
    /// Photos sent together share this; they get one reply, not one each.
    album: Option<String>,
    /// A voice note, audio file or photo, taken into the inbox when there
    /// is one (M41, M47), and the caption that came with it.
    file: Option<TgFile>,
}

/// A voice note, audio file or photo in an update.
struct TgFile {
    file_id: String,
    name: String,
    mime: Option<String>,
    size: Option<u64>,
    caption: String,
    /// For a photo: every size Telegram offers (`file_id`, `file_size`,
    /// `width`, `height`), smallest first. The one taken is chosen against
    /// the inbox's cap, which the parser doesn't know.
    sizes: Vec<Value>,
}

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
    poll_deadline: Duration,
    backoff_min: Duration,
    backoff_max: Duration,
    /// When `getUpdates` last answered ok (M19b: `/status`, the watchdog).
    last_ok_poll: Mutex<Option<SystemTime>>,
    /// Where notices about the bot itself go (M19c): a conflict, a
    /// removed webhook.
    owner: Option<i64>,
    /// What `setMyCommands` registers (M48).
    menu: Vec<crate::menu::Command>,
    conflict_after: Duration,
    conflict: Mutex<Option<Conflict>>,
    /// When each ignored chat was last logged.
    ignored: Mutex<HashMap<i64, Instant>>,
    /// Albums already told their photos weren't read.
    albums: Mutex<HashSet<String>>,
    /// Albums whose first photo was taken (M47): the rest are dropped.
    taken: Mutex<HashSet<String>>,
    /// Where voice notes and audio files are saved (M41); `None` leaves
    /// them unread, as before.
    inbox: Option<Inbox>,
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
        builder = builder
            .connect_timeout(CONNECT_DEADLINE)
            .timeout(REQUEST_DEADLINE)
            .tcp_keepalive(Duration::from_secs(30));
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
            client: builder.build().expect("reqwest client"),
            offset: AtomicI64::new(0),
            allowed_chats: Vec::new(),
            told: Mutex::new(HashSet::new()),
            poll_deadline: POLL_DEADLINE,
            backoff_min: BACKOFF_MIN,
            backoff_max: BACKOFF_MAX,
            last_ok_poll: Mutex::new(None),
            owner: None,
            menu: crate::menu::BUILT_IN.to_vec(),
            conflict_after: CONFLICT_AFTER,
            conflict: Mutex::new(None),
            ignored: Mutex::new(HashMap::new()),
            albums: Mutex::new(HashSet::new()),
            taken: Mutex::new(HashSet::new()),
            inbox: None,
        }
    }

    /// Saves voice notes and audio files people send here, so the gateway
    /// can transcribe them (M41). Other files keep the "only text" reply.
    pub fn with_inbox(mut self, inbox: Option<Inbox>) -> Self {
        self.inbox = inbox;
        self
    }

    /// Tests only: short deadlines and backoff so a hung or failing mock
    /// server shows up in milliseconds, not minutes.
    #[cfg(test)]
    fn with_fast_retries(mut self, poll_deadline: Duration) -> Self {
        self.poll_deadline = poll_deadline;
        self.backoff_min = Duration::from_millis(20);
        self.backoff_max = Duration::from_millis(100);
        self
    }

    /// The chats this bot answers; everyone else is ignored.
    pub fn with_allowed_chats(mut self, chats: Vec<i64>) -> Self {
        self.allowed_chats = chats;
        self
    }

    /// The chat told about trouble with the bot itself: a lasting 409, a
    /// webhook that was removed. `None`: it's only logged.
    pub fn with_owner(mut self, chat: Option<i64>) -> Self {
        self.owner = chat;
        self
    }

    /// The commands under the `/` button: the owner's private chat gets
    /// all of them, a group only the ones anyone may send.
    pub fn with_menu(mut self, cmds: Vec<crate::menu::Command>) -> Self {
        self.menu = cmds;
        self
    }

    /// How long 409s go on before the owner is told ([`CONFLICT_AFTER`]).
    pub fn with_conflict_after(mut self, after: Duration) -> Self {
        self.conflict_after = after;
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
        let due = {
            let mut ignored = self.ignored.lock().unwrap();
            let due = ignored
                .get(&chat)
                .is_none_or(|at| at.elapsed() >= IGNORED_WARN_EVERY);
            if due {
                ignored.insert(chat, Instant::now());
            }
            due
        };
        if due {
            tracing::warn!(
                sender = %msg.sender,
                "telegram: ignored a message from chat {chat}: it isn't in telegram_allowed_chats — add it there if this chat should reach the agent (logged once an hour per chat)"
            );
        }
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

    /// One `getUpdates` round: forwards what's admitted, advances the offset.
    /// `Ok(false)` means the receiving side is gone and the adapter is done.
    async fn poll_once(&self, tx: &mpsc::Sender<InboundMessage>) -> Result<bool, PollError> {
        let offset = self.offset.load(Ordering::SeqCst);
        let url = format!(
            "{}?timeout={LONG_POLL_SECS}&offset={offset}&allowed_updates=%5B%22message%22%2C%22callback_query%22%5D",
            self.api_url("getUpdates")
        );
        let resp = self
            .client
            .get(&url)
            .timeout(self.poll_deadline)
            .send()
            .await
            .map_err(|e| {
                PollError::Retry(
                    format!("getUpdates request failed: {}", e.without_url()),
                    None,
                )
            })?;
        let status = resp.status();
        let body: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                return Err(PollError::Retry(
                    format!(
                        "getUpdates: bad json (status {status}): {}",
                        e.without_url()
                    ),
                    None,
                ))
            }
        };
        if !body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let code = body
                .get("error_code")
                .and_then(|v| v.as_u64())
                .unwrap_or(u64::from(status.as_u16()));
            let msg = format!("getUpdates returned not-ok: {body}");
            // 401: the token is wrong or revoked; 404: the URL has no valid
            // bot in it. Both need the owner, not another try.
            if code == 401 || code == 404 {
                return Err(PollError::Fatal(msg));
            }
            if code == 409 {
                let description = body
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Conflict")
                    .to_string();
                return Err(PollError::Conflict(description));
            }
            let wait = body
                .get("parameters")
                .and_then(|p| p.get("retry_after"))
                .and_then(|v| v.as_u64())
                .map(Duration::from_secs);
            return Err(PollError::Retry(msg, wait));
        }
        *self.last_ok_poll.lock().unwrap() = Some(SystemTime::now());
        let updates = body
            .get("result")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();
        for update in &updates {
            if let Some(update_id) = update.get("update_id").and_then(|v| v.as_i64()) {
                self.offset.store(update_id + 1, Ordering::SeqCst);
            }
            if let Some(id) = update["callback_query"]["id"].as_str() {
                // Stops the button's spinner; the tap itself is handled
                // like typed text below.
                self.answer_callback(id);
            }
            if let Some(parsed) = Self::parse_update(update) {
                if !self.admits(&parsed.msg).await {
                    continue;
                }
                if let Some(q) = update.get("callback_query") {
                    self.mark_choice(q).await;
                }
                let Some(parsed) = self.take_file(parsed).await else {
                    continue;
                };
                if let Some(kind) = parsed.unread.filter(|_| parsed.msg.text.is_empty()) {
                    self.cannot_read(&parsed, kind).await;
                    continue;
                }
                if tx.send(parsed.msg).await.is_err() {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// A voice note, audio file or photo, saved to the inbox: the message
    /// then carries it as an attachment with M39's line about it, and is
    /// read. `None`: the photo is one more of an album already taken, and
    /// was answered here.
    async fn take_file(&self, mut parsed: Parsed) -> Option<Parsed> {
        let (Some(inbox), Some(mut file)) = (&self.inbox, parsed.file.take()) else {
            return Some(parsed);
        };
        if !file.sizes.is_empty() {
            if let Some(album) = &parsed.album {
                let first = {
                    let mut taken = self.taken.lock().unwrap();
                    if taken.len() > 1000 {
                        taken.clear();
                    }
                    taken.insert(album.clone())
                };
                if !first {
                    self.once_per_album(
                        &parsed,
                        "more",
                        "I only looked at the first photo of that album. Send the others one at a time if you want them read.",
                    )
                    .await;
                    return None;
                }
            }
            match photo_size(&file.sizes, inbox.max_bytes()) {
                Some((id, size)) => {
                    file.file_id = id;
                    file.size = size;
                }
                None => {
                    // The whole photo set is over the cap (or absurd in
                    // pixels): say why, read the caption.
                    let refused = vec![
                        match file.sizes.last().and_then(|p| p["file_size"].as_u64()) {
                            Some(n) => inbox.too_big(&file.name, n),
                            None => Refused {
                                name: file.name.clone(),
                                why: "Telegram offered no size of it small enough".into(),
                            },
                        },
                    ];
                    parsed.msg.text = files::with_notes(&file.caption, &[], &refused);
                    parsed.unread = None;
                    return Some(parsed);
                }
            }
        }
        let (saved, refused) = match self.download(inbox, &parsed.msg.message_id, &file).await {
            Ok(s) => (vec![s], vec![]),
            Err(r) => (vec![], vec![r]),
        };
        parsed.msg.attachments = saved
            .iter()
            .map(|s| Attachment {
                kind: s.mime.clone(),
                url: s.path.to_string_lossy().into_owned(),
                name: Some(s.rel.clone()),
            })
            .collect();
        parsed.msg.text = files::with_notes(&file.caption, &saved, &refused);
        parsed.unread = None;
        Some(parsed)
    }

    /// `getFile`, then the file itself, capped at the inbox's size.
    async fn download(
        &self,
        inbox: &Inbox,
        message_id: &str,
        tg: &TgFile,
    ) -> Result<Saved, Refused> {
        let refuse = |why: String| Refused {
            name: tg.name.clone(),
            why,
        };
        if let Some(n) = tg.size.filter(|n| *n > inbox.max_bytes()) {
            return Err(inbox.too_big(&tg.name, n));
        }
        let file = self
            .call("getFile", json!({ "file_id": tg.file_id }))
            .await
            .map_err(|e| refuse(format!("Telegram didn't hand it over ({e})")))?;
        let Some(path) = file.get("file_path").and_then(Value::as_str) else {
            return Err(refuse(
                "Telegram didn't hand it over (no file_path; bots can fetch files up to 20 MB)"
                    .into(),
            ));
        };
        let url = format!("{}/file/bot{}/{}", self.base_url, self.token, path);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| refuse(format!("the download failed: {}", e.without_url())))?;
        if !resp.status().is_success() {
            return Err(refuse(format!(
                "the download failed (HTTP {})",
                resp.status()
            )));
        }
        let bytes = files::read_capped(resp, inbox.max_bytes())
            .await
            .map_err(refuse)?;
        inbox
            .save("telegram", message_id, &tg.name, tg.mime.as_deref(), &bytes)
            .map_err(|e| refuse(format!("it couldn't be saved: {e}")))
    }

    fn api_url(&self, method: &str) -> String {
        format!("{}/bot{}/{}", self.base_url, self.token, method)
    }

    /// A message with nothing to read in it: one plain reply (one per
    /// album), so the sender isn't left waiting on an answer that won't come.
    async fn cannot_read(&self, parsed: &Parsed, kind: &str) {
        tracing::info!(
            chat = %parsed.msg.chat_id,
            "telegram: got a {kind} with no text; ferrule reads only text"
        );
        let text = format!(
            "I got your {kind}, but I can only read text for now, so I don't know what's in it. Please type your message instead."
        );
        self.once_per_album(parsed, "unread", &text).await;
    }

    /// `text` as a reply to `parsed`, once per album (`what` tells the
    /// kinds of reply apart) or, outside an album, every time.
    async fn once_per_album(&self, parsed: &Parsed, what: &str, text: &str) {
        if let Some(album) = &parsed.album {
            let mut albums = self.albums.lock().unwrap();
            if albums.len() > 1000 {
                albums.clear();
            }
            if !albums.insert(format!("{what}:{album}")) {
                return;
            }
        }
        let text = text.to_string();
        let reply = OutboundMessage {
            channel: "telegram".into(),
            chat_id: parsed.msg.chat_id.clone(),
            text,
            reply_to: Some(parsed.msg.message_id.clone()),
            attachments: vec![],
        };
        if let Err(e) = self.send(reply).await {
            tracing::warn!(error = %e, "telegram: couldn't send a reply about {what}");
        }
    }

    /// Something the owner has to hear about the bot itself.
    async fn tell_owner(&self, text: String) {
        let Some(owner) = self.owner else {
            tracing::warn!("telegram: no owner chat to tell: {text}");
            return;
        };
        let msg = OutboundMessage {
            channel: "telegram".into(),
            chat_id: owner.to_string(),
            text,
            reply_to: None,
            attachments: vec![],
        };
        if let Err(e) = self.send(msg).await {
            tracing::warn!(error = %e, "telegram: couldn't tell the owner");
        }
    }

    /// One Bot API call; `Err` says why in words, never with the URL.
    async fn call(&self, method: &str, payload: Value) -> Result<Value, String> {
        let resp = self
            .client
            .post(self.api_url(method))
            .json(&payload)
            .send()
            .await
            .map_err(|e| format!("{method} request failed: {}", e.without_url()))?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(json!({}));
        if !body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Err(format!("{method} failed (status {status}): {body}"));
        }
        Ok(body.get("result").cloned().unwrap_or(Value::Null))
    }

    /// M41: the command menu under the chat's `/` button. Best effort: a
    /// bot without it works the same, so a failure is only logged.
    async fn set_commands(&self) {
        for body in crate::menu::telegram_scopes(&self.menu, self.owner) {
            let scope = body["scope"]["type"].as_str().unwrap_or("").to_string();
            let call = self.call("setMyCommands", body);
            match tokio::time::timeout(Duration::from_secs(5), call).await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, scope, "telegram: couldn't set the command menu")
                }
                Err(_) => tracing::warn!(scope, "telegram: setting the command menu timed out"),
            }
        }
    }

    /// A webhook set on the bot makes Telegram push updates there and
    /// refuse `getUpdates` (409), so the bot goes deaf. Removed, keeping
    /// the updates waiting at Telegram, and the owner is told. Returns
    /// whether one was removed.
    async fn remove_webhook(&self) -> bool {
        let info = match self.call("getWebhookInfo", json!({})).await {
            Ok(info) => info,
            Err(e) => {
                tracing::warn!(error = %e, "telegram: couldn't check for a webhook");
                return false;
            }
        };
        let url = info.get("url").and_then(|v| v.as_str()).unwrap_or("");
        if url.is_empty() {
            return false;
        }
        // Only the host: a webhook's path is often its secret.
        let host = url
            .split("://")
            .nth(1)
            .unwrap_or(url)
            .split(['/', '?', '#'])
            .next()
            .unwrap_or("")
            .rsplit('@')
            .next()
            .unwrap_or("")
            .to_string();
        if let Err(e) = self
            .call("deleteWebhook", json!({"drop_pending_updates": false}))
            .await
        {
            tracing::warn!(error = %e, host, "telegram: a webhook is set on the bot and couldn't be removed; no message will arrive until it is");
            self.tell_owner(format!(
                "Your bot has a webhook set (to {host}), so Telegram sends its messages there instead of to me, and I couldn't remove it: {e}. Remove it with `ferrule setup` → Telegram, or stop the service that set it."
            ))
            .await;
            return false;
        }
        tracing::warn!(
            host,
            "telegram: removed a webhook set on the bot; it kept messages from reaching ferrule"
        );
        self.tell_owner(format!(
            "Your bot had a webhook set (to {host}), so Telegram was sending its messages there instead of to me. I removed it so I can receive them; messages already waiting at Telegram were kept. If another service needs that webhook, it and ferrule can't share this bot token: give one of them its own bot from @BotFather."
        ))
        .await;
        true
    }

    /// A 409 from `getUpdates`: remove a webhook if that's the cause, and
    /// tell the owner once the conflict has lasted `conflict_after`.
    async fn on_conflict(&self, description: &str) {
        if description.to_ascii_lowercase().contains("webhook") && self.remove_webhook().await {
            return;
        }
        let now = Instant::now();
        let tell = {
            let mut conflict = self.conflict.lock().unwrap();
            let c = conflict.get_or_insert_with(|| Conflict {
                since: now,
                last: now,
                description: String::new(),
                told: false,
            });
            c.last = now;
            c.description = description.to_string();
            let due = !c.told && now.duration_since(c.since) >= self.conflict_after;
            c.told |= due;
            due.then(|| now.duration_since(c.since))
        };
        if let Some(lasted) = tell {
            tracing::warn!(
                description,
                "telegram: getUpdates has been refused with 409 Conflict for {}; told the owner",
                human(lasted)
            );
            self.tell_owner(conflict_notice(description, lasted)).await;
        }
    }

    /// A good poll: a conflict that has stayed away `conflict_after` is over.
    async fn on_ok_poll(&self) {
        let over = {
            let mut conflict = self.conflict.lock().unwrap();
            match conflict.as_ref() {
                Some(c) if c.last.elapsed() >= self.conflict_after => conflict.take(),
                _ => None,
            }
        };
        if let Some(c) = over {
            let lasted = c.last.duration_since(c.since);
            tracing::info!(
                "telegram: the 409 Conflict is over; it lasted {}",
                human(lasted)
            );
            if c.told {
                self.tell_owner(format!(
                    "I'm getting this bot's messages again: the 409 Conflict cleared (it lasted {}). Messages the other program fetched meanwhile went to it, not to me.",
                    human(lasted)
                ))
                .await;
            }
        }
    }

    fn answer_callback(&self, id: &str) {
        let req = self
            .client
            .post(self.api_url("answerCallbackQuery"))
            .json(&json!({ "callback_query_id": id }));
        tokio::spawn(async move {
            if let Err(e) = req.send().await {
                tracing::warn!(error = %e.without_url(), "telegram: answerCallbackQuery failed");
            }
        });
    }

    /// A tap marks its choice, as on Discord and Slack: the message's
    /// buttons give way to "→ <the tapped button's label>", so a question
    /// can't be answered twice and the chat shows what was picked. Best
    /// effort — the tap is forwarded even when the edit fails.
    async fn mark_choice(&self, q: &Value) {
        let Some(message) = q.get("message") else {
            return;
        };
        let Some(rows) = message["reply_markup"]["inline_keyboard"].as_array() else {
            return;
        };
        let (Some(chat_id), Some(message_id)) = (
            message["chat"]["id"].as_i64(),
            message["message_id"].as_i64(),
        ) else {
            return;
        };
        // Telegram's callback carries only the button's data, not its
        // label; the label is found on the message's keyboard.
        let data = q["data"].as_str().unwrap_or("");
        let label = rows
            .iter()
            .flat_map(|row| row.as_array().into_iter().flatten())
            .find(|b| b["callback_data"].as_str() == Some(data))
            .and_then(|b| b["text"].as_str())
            .unwrap_or("done");
        let no_buttons = json!({ "inline_keyboard": [] });
        let result = match message["text"].as_str() {
            Some(text) => {
                let marker = format!("\n→ {label}");
                let room = MESSAGE_LIMIT.saturating_sub(marker.encode_utf16().count());
                let base = chunks(text, room).into_iter().next().unwrap_or_default();
                self.deliver(
                    "editMessageText",
                    &json!({ "chat_id": chat_id, "message_id": message_id,
                             "text": format!("{base}{marker}"), "reply_markup": no_buttons }),
                )
                .await
            }
            // The message has no text to mark (a caption, say): the
            // keyboard still gives way.
            None => {
                self.deliver(
                    "editMessageReplyMarkup",
                    &json!({ "chat_id": chat_id, "message_id": message_id, "reply_markup": no_buttons }),
                )
                .await
            }
        };
        if let Err(e) = result {
            tracing::warn!(error = %e, "telegram: couldn't mark a tapped button");
        }
    }

    fn parse_update(update: &Value) -> Option<Parsed> {
        if let Some(q) = update.get("callback_query") {
            return Self::parse_callback(q).map(|msg| Parsed {
                msg,
                unread: None,
                album: None,
                file: None,
            });
        }
        let message = update.get("message")?;
        let chat_id = message.get("chat")?.get("id")?.as_i64()?.to_string();
        let message_id = message.get("message_id")?.as_i64()?.to_string();
        let unread = unread_kind(message);
        let text = match (message.get("text"), message.get("caption"), unread) {
            (Some(text), _, _) => text.as_str()?.to_string(),
            (None, Some(caption), Some(kind)) => format!(
                "{}\n\n[The {kind} attached to this message wasn't read: ferrule reads only text for now.]",
                caption.as_str()?
            ),
            // Nothing to read: the sender is told, the agent isn't asked.
            (None, _, Some(_)) => String::new(),
            // Someone joined, a message was pinned: nothing to answer.
            _ => return None,
        };
        let sender = message
            .get("from")
            .and_then(|f| f.get("username").or_else(|| f.get("first_name")))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let sender_id = message
            .get("from")
            .and_then(|f| f.get("id"))
            .and_then(|v| v.as_i64())
            .map(|id| id.to_string());
        let ts = message.get("date").and_then(|d| d.as_i64()).unwrap_or(0);
        let album = message
            .get("media_group_id")
            .and_then(|v| v.as_str())
            .map(|g| format!("{chat_id}/{g}"));
        Some(Parsed {
            msg: InboundMessage {
                channel: "telegram".into(),
                chat_id,
                sender,
                sender_id,
                message_id,
                text,
                attachments: vec![],
                reply_to: None,
                ts,
            },
            unread,
            album,
            file: tg_file(message),
        })
    }
}

/// The photo sizes Telegram offers, and the largest one worth taking: at
/// most 2000 px on a side (a model downscales past that anyway) and within
/// `cap` bytes. Its `file_id` and size.
fn photo_size(sizes: &[Value], cap: u64) -> Option<(String, Option<u64>)> {
    sizes
        .iter()
        .rfind(|p| {
            let side = p["width"].as_u64().max(p["height"].as_u64()).unwrap_or(0);
            side <= 2000 && p["file_size"].as_u64().is_none_or(|n| n <= cap)
        })
        .and_then(|p| Some((p["file_id"].as_str()?.to_string(), p["file_size"].as_u64())))
}

/// The voice note, audio file or photo in a message, if it has one.
fn tg_file(message: &Value) -> Option<TgFile> {
    let caption = || {
        message
            .get("caption")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    if let Some(sizes) = message.get("photo").and_then(Value::as_array) {
        return Some(TgFile {
            file_id: String::new(),
            name: "photo.jpg".into(),
            mime: Some("image/jpeg".into()),
            size: None,
            caption: caption(),
            sizes: sizes.clone(),
        });
    }
    let (key, a) = ["voice", "audio"]
        .into_iter()
        .find_map(|k| message.get(k).map(|a| (k, a)))?;
    let file_id = a.get("file_id")?.as_str()?.to_string();
    let mime = a
        .get("mime_type")
        .and_then(Value::as_str)
        .map(str::to_string);
    let name = match (key, a.get("file_name").and_then(Value::as_str)) {
        (_, Some(n)) => n.to_string(),
        // A voice note is Ogg Opus with no name.
        ("voice", None) => "voice.ogg".to_string(),
        (_, None) => "audio".to_string(),
    };
    Some(TgFile {
        file_id,
        name,
        mime,
        size: a.get("file_size").and_then(Value::as_u64),
        caption: caption(),
        sizes: vec![],
    })
}

/// What a message carries that ferrule can't read, in words for the
/// sender. The order matters: a GIF also has a `document`.
fn unread_kind(message: &Value) -> Option<&'static str> {
    [
        ("voice", "voice message"),
        ("video_note", "video message"),
        ("audio", "audio file"),
        ("photo", "photo"),
        ("animation", "GIF"),
        ("video", "video"),
        ("sticker", "sticker"),
        ("document", "file"),
    ]
    .into_iter()
    .find(|(key, _)| message.get(key).is_some())
    .map(|(_, kind)| kind)
}

/// The owner's message about a lasting 409, the likely cause first.
fn conflict_notice(description: &str, lasted: Duration) -> String {
    let webhook = description.to_ascii_lowercase().contains("webhook");
    let (first, second) = if webhook {
        (
            "a webhook is set on the bot, so Telegram sends its messages there (I tried to remove it and couldn't)",
            "another program polling with this bot's token",
        )
    } else {
        (
            "another program is fetching this bot's messages with the same token — most likely a second ferrule gateway (another machine, an old service, a terminal left running) or another bot program",
            "a webhook set on the bot (I remove one when I find it)",
        )
    };
    format!(
        "I'm not getting this bot's messages: Telegram has refused them to me for {} (409 Conflict: \"{}\"). The cause: {first}. Less likely: {second}. Stop the other one and I'll pick up again by myself. `ferrule doctor` on each machine shows whether a gateway runs there.",
        human(lasted),
        crate::health::clip(description, 200)
    )
}

/// The fence line (```` ``` ```` or `~~~`, with its info string) open at
/// `upto`, given the fence open at the text's start. Toggled by any line
/// that is one: a close has no info string.
fn fence_at(text: &str, upto: usize, mut open: Option<String>) -> Option<String> {
    for line in text[..upto].split('\n') {
        let line = line.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            open = match open {
                None => Some(line.to_string()),
                Some(_) => None,
            };
        }
    }
    open
}

/// `text` cut into pieces Telegram takes (at most [`MESSAGE_LIMIT`] UTF-16
/// units each), at safe boundaries: a blank line in the piece's second
/// half, else a line end or a space near its end, else a hard cut. A
/// piece that ends inside a fenced code block closes the fence and the
/// next piece reopens it, so a long `/plan` renders whole in every piece.
/// Never empty: an empty text is one empty piece.
fn split(text: &str) -> Vec<String> {
    let units = |s: &str| s.encode_utf16().count();
    if units(text) <= MESSAGE_LIMIT {
        return vec![text.to_string()];
    }
    let mut pieces = Vec::new();
    let mut rest = text.to_string();
    // The fence open at the start of `rest` (a piece cut inside a code
    // block reopens it here).
    let mut fence: Option<String> = None;
    loop {
        // Room to close a fence ("\n```") should the cut land inside one.
        let budget = MESSAGE_LIMIT - 4;
        let mut used = 0;
        let hard = rest
            .char_indices()
            .find_map(|(i, c)| {
                used += c.len_utf16();
                (used > budget).then_some(i)
            })
            .expect("rest is longer than the budget");
        let head = &rest[..hard];
        let half = head
            .char_indices()
            .nth(head.chars().count() / 2)
            .map_or(0, |(i, _)| i);
        let fifth = head
            .char_indices()
            .nth(head.chars().count() * 4 / 5)
            .map_or(0, |(i, _)| i);
        let cut = head[half..]
            .rfind("\n\n")
            .map(|i| half + i + 2)
            .or_else(|| head[fifth..].rfind('\n').map(|i| fifth + i + 1))
            .or_else(|| head[fifth..].rfind(' ').map(|i| fifth + i + 1))
            .unwrap_or(hard);
        let mut piece = rest[..cut].trim_end().to_string();
        let (cut, in_fence) = if piece.is_empty() {
            // Nothing but whitespace before the cut: keep the text.
            (hard, fence_at(&rest, hard, fence.clone()))
        } else {
            (cut, fence_at(&rest, cut, fence.clone()))
        };
        if piece.is_empty() {
            piece = rest[..cut].to_string();
        }
        rest = match in_fence {
            Some(open) => {
                piece.push_str("\n```");
                fence = None;
                format!("{open}\n{}", &rest[cut.min(rest.len())..])
            }
            None => {
                fence = None;
                rest[cut..].to_string()
            }
        };
        pieces.push(piece);
        if units(&rest) <= MESSAGE_LIMIT {
            pieces.push(rest);
            return pieces;
        }
    }
}

impl TelegramChannel {
    /// A button tap (M20): its data arrives as if typed in the chat the
    /// button's message is in, by whoever tapped it. It's only ever the
    /// command the button carried, and trusted no more than typed text.
    fn parse_callback(q: &Value) -> Option<InboundMessage> {
        let message = q.get("message")?;
        let chat_id = message.get("chat")?.get("id")?.as_i64()?.to_string();
        let text = q.get("data")?.as_str()?.to_string();
        let sender = q
            .get("from")
            .and_then(|f| f.get("username").or_else(|| f.get("first_name")))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        // Whoever tapped: Telegram's own id for them, as for a typed message.
        let sender_id = q
            .get("from")
            .and_then(|f| f.get("id"))
            .and_then(|v| v.as_i64())
            .map(|id| id.to_string());
        Some(InboundMessage {
            channel: "telegram".into(),
            chat_id,
            sender,
            sender_id,
            // No message of the owner's to react to or reply to.
            message_id: String::new(),
            text,
            attachments: vec![],
            reply_to: None,
            ts: message.get("date").and_then(|d| d.as_i64()).unwrap_or(0),
        })
    }

    /// An inline keyboard, one button per row. `callback_data` holds at
    /// most 64 bytes; a longer command can't be a button.
    fn keyboard(buttons: &[Button]) -> Result<Value, GatewayError> {
        let rows = buttons
            .iter()
            .map(|b| match &b.action {
                ButtonAction::Url(url) => Ok(json!([{ "text": b.text, "url": url }])),
                ButtonAction::Command(cmd) if cmd.len() <= 64 => {
                    Ok(json!([{ "text": b.text, "callback_data": cmd }]))
                }
                ButtonAction::Command(_) => Err(GatewayError::Unsupported(
                    "a button command longer than 64 bytes",
                )),
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(json!({ "inline_keyboard": rows }))
    }

    /// `sendMessage`; the new message's id.
    async fn post_message(&self, payload: Value) -> Result<Option<String>, GatewayError> {
        let result = self.deliver("sendMessage", &payload).await?;
        Ok(result
            .get("message_id")
            .and_then(|v| v.as_i64())
            .map(|id| id.to_string()))
    }

    /// A call that puts something in a chat. A 429 is
    /// `GatewayError::RateLimited` with Telegram's `retry_after` (M27: a
    /// streamed reply waits it out); any other failure names the method.
    async fn deliver(&self, method: &str, payload: &Value) -> Result<Value, GatewayError> {
        let resp = self
            .client
            .post(self.api_url(method))
            .json(payload)
            .send()
            .await
            .map_err(|e| {
                GatewayError::Channel(format!("{method} request failed: {}", e.without_url()))
            })?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(json!({}));
        if status.is_success() && body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Ok(body.get("result").cloned().unwrap_or(Value::Null));
        }
        let code = body.get("error_code").and_then(|v| v.as_u64());
        if status.as_u16() == 429 || code == Some(429) {
            let secs = body
                .get("parameters")
                .and_then(|p| p.get("retry_after"))
                .and_then(|v| v.as_u64())
                .unwrap_or(1);
            return Err(GatewayError::RateLimited {
                retry_after: Duration::from_secs(secs),
            });
        }
        Err(GatewayError::Channel(format!(
            "{method} failed (status {status}): {body}"
        )))
    }

    /// `sendMessage` for each piece of `msg` ([`split`] at
    /// [`MESSAGE_LIMIT`], as Discord and Slack do for their caps): a
    /// reply-to rides the first piece, the buttons the last, and the last
    /// piece's id is the answer (the one a streamed reply edits).
    async fn deliver_message(
        &self,
        msg: &OutboundMessage,
        buttons: Option<&[Button]>,
    ) -> Result<Option<String>, GatewayError> {
        let keyboard = buttons.map(Self::keyboard).transpose()?;
        let pieces = split(&msg.text);
        let last = pieces.len() - 1;
        let mut id = None;
        for (i, piece) in pieces.iter().enumerate() {
            let mut payload = json!({ "chat_id": msg.chat_id, "text": piece });
            if i == 0 {
                if let Some(reply_to) = &msg.reply_to {
                    if let Ok(reply_id) = reply_to.parse::<i64>() {
                        payload["reply_to_message_id"] = json!(reply_id);
                    }
                }
            }
            if i == last {
                if let Some(keyboard) = &keyboard {
                    payload["reply_markup"] = keyboard.clone();
                }
            }
            id = self.post_message(payload).await?;
        }
        Ok(id)
    }
}

#[async_trait::async_trait]
impl Channel for TelegramChannel {
    fn name(&self) -> &str {
        "telegram"
    }

    /// `sendChatAction typing`, which shows for 5 s: again every 4.
    async fn typing(&self, chat_id: &str, _message_id: &str, on: bool) -> Typing {
        if !on {
            return Typing::Unsupported;
        }
        let action = json!({ "chat_id": chat_id, "action": "typing" });
        match self.deliver("sendChatAction", &action).await {
            Ok(_) => Typing::Shown {
                again_in: Duration::from_secs(4),
            },
            Err(GatewayError::RateLimited { .. }) => Typing::Limited,
            Err(e) => {
                tracing::debug!(error = %e, "telegram: typing");
                Typing::Failed
            }
        }
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            reactions: true,
            edits: true,
            buttons: true,
            ..Default::default()
        }
    }

    fn polls(&self) -> bool {
        true
    }

    fn last_ok_poll(&self) -> Option<SystemTime> {
        *self.last_ok_poll.lock().unwrap()
    }

    fn problem(&self) -> Option<String> {
        let conflict = self.conflict.lock().unwrap();
        let c = conflict.as_ref()?;
        let told = if c.told { "; the owner was told" } else { "" };
        Some(format!(
            "409 Conflict since {} ago (another program polling with this token, or a webhook){told}: {}",
            human(c.since.elapsed()),
            crate::health::clip(&c.description, 120)
        ))
    }

    /// Blocks forever, long-polling `getUpdates`. A failed poll — the
    /// network, a deadline, Telegram's own trouble — is logged and retried
    /// with backoff (1s doubling to 60s, reset by the next good poll), so a
    /// blip never leaves the bot deaf. Returns `Err` only when Telegram
    /// rejects the token, and `Ok` once the receiving side of `tx` is gone.
    async fn run(&self, tx: mpsc::Sender<InboundMessage>) -> Result<(), GatewayError> {
        self.remove_webhook().await;
        self.set_commands().await;
        let mut backoff = self.backoff_min;
        let mut failures = 0u32;
        loop {
            match self.poll_once(&tx).await {
                Ok(true) => {
                    if failures > 0 {
                        tracing::info!(failures, "telegram: polling recovered");
                    }
                    failures = 0;
                    backoff = self.backoff_min;
                    self.on_ok_poll().await;
                }
                Err(PollError::Conflict(description)) => {
                    failures += 1;
                    tracing::warn!(
                        failures,
                        retry_in_ms = backoff.as_millis() as u64,
                        "telegram: getUpdates got 409 Conflict (another program polling with this token, or a webhook): {description}"
                    );
                    self.on_conflict(&description).await;
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(self.backoff_max);
                }
                Ok(false) => return Ok(()),
                Err(PollError::Fatal(e)) => {
                    tracing::error!(error = %e, "telegram: the bot token was rejected, stopping");
                    return Err(GatewayError::Channel(e));
                }
                Err(PollError::Retry(e, retry_after)) => {
                    failures += 1;
                    let wait = retry_after.unwrap_or(backoff);
                    tracing::warn!(error = %e, failures, retry_in_ms = wait.as_millis() as u64, "telegram: poll failed, retrying");
                    tokio::time::sleep(wait).await;
                    backoff = (backoff * 2).min(self.backoff_max);
                }
            }
        }
    }

    async fn send(&self, msg: OutboundMessage) -> Result<(), GatewayError> {
        self.deliver_message(&msg, None).await.map(|_| ())
    }

    async fn post(&self, msg: OutboundMessage) -> Result<Option<String>, GatewayError> {
        self.deliver_message(&msg, None).await
    }

    async fn send_buttons(
        &self,
        msg: OutboundMessage,
        buttons: &[Button],
    ) -> Result<(), GatewayError> {
        self.deliver_message(&msg, Some(buttons)).await.map(|_| ())
    }

    /// `setMessageReaction`: the 👀 receipt (M19b).
    async fn react(
        &self,
        chat_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<(), GatewayError> {
        let Ok(id) = message_id.parse::<i64>() else {
            return Err(GatewayError::Channel(format!(
                "setMessageReaction: not a message id: {message_id}"
            )));
        };
        let payload = json!({
            "chat_id": chat_id,
            "message_id": id,
            "reaction": [{"type": "emoji", "emoji": emoji}],
        });
        let resp = self
            .client
            .post(self.api_url("setMessageReaction"))
            .json(&payload)
            .send()
            .await
            .map_err(|e| {
                GatewayError::Channel(format!(
                    "setMessageReaction request failed: {}",
                    e.without_url()
                ))
            })?;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap_or(json!({}));
        if !status.is_success() || !body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Err(GatewayError::Channel(format!(
                "setMessageReaction failed (status {status}): {body}"
            )));
        }
        Ok(())
    }

    async fn edit(&self, chat_id: &str, message_id: &str, text: &str) -> Result<(), GatewayError> {
        let payload = json!({ "chat_id": chat_id, "message_id": message_id.parse::<i64>().unwrap_or(0), "text": text });
        self.deliver("editMessageText", &payload).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_photo_takes_the_largest_size_a_model_can_use_within_the_cap() {
        let size = |id: &str, w: u64, n: u64| json!({"file_id": id, "width": w, "height": w / 2, "file_size": n});
        let sizes = vec![
            size("s", 90, 1_000),
            size("m", 320, 20_000),
            size("x", 1280, 300_000),
            size("y", 2560, 900_000),
        ];
        // 2560 px is more than a model looks at; 1280 is the one.
        assert_eq!(
            photo_size(&sizes, 20_000_000),
            Some(("x".into(), Some(300_000)))
        );
        // A tighter cap steps down to what fits.
        assert_eq!(photo_size(&sizes, 100_000).unwrap().0, "m");
        // Nothing fits: there is no size to fetch (the caller says why).
        assert_eq!(photo_size(&sizes, 500), None);
        assert_eq!(photo_size(&[], 1_000_000), None);
        // A size that doesn't say how big it is is still taken.
        let unsized_ = vec![json!({"file_id": "u", "width": 800, "height": 600})];
        assert_eq!(photo_size(&unsized_, 10), Some(("u".into(), None)));
    }

    #[test]
    fn a_photo_message_is_a_jpeg_with_its_caption_and_sizes() {
        let m = json!({"photo": [{"file_id": "a", "width": 90, "height": 60}], "caption": "what is this?"});
        let f = tg_file(&m).unwrap();
        assert_eq!(f.name, "photo.jpg");
        assert_eq!(f.mime.as_deref(), Some("image/jpeg"));
        assert_eq!(f.caption, "what is this?");
        assert_eq!(f.sizes.len(), 1);
        assert!(tg_file(&json!({"text": "hi"})).is_none());
    }
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
                } else if request.contains("sendMessage")
                    || request.contains("editMessageText")
                    || request.contains("setMessageReaction")
                {
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
        channel.react("9999", "55", "👀").await.unwrap();
        assert!(channel.last_ok_poll().is_some());

        let sent_bodies = sent.lock().unwrap();
        assert_eq!(sent_bodies.len(), 3);
        assert_eq!(sent_bodies[0]["text"], "reply");
        assert_eq!(sent_bodies[0]["chat_id"], "9999");
        assert_eq!(sent_bodies[0]["reply_to_message_id"], 55);
        assert_eq!(sent_bodies[1]["text"], "edited text");
        assert_eq!(sent_bodies[1]["message_id"], 55);
        assert_eq!(
            sent_bodies[2],
            json!({"chat_id": "9999", "message_id": 55,
                   "reaction": [{"type": "emoji", "emoji": "👀"}]})
        );
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
            sender_id: None,
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

    /// A Bot API mock for M20's buttons: the first `getUpdates` is a
    /// button tap from `chat`; every call is kept as (method, body/query).
    /// (method, body) of every call the mock saw.
    type Calls = Arc<Mutex<Vec<(String, String)>>>;

    fn button_server(chat: i64) -> (String, Calls) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let kept = calls.clone();
        let served = Arc::new(AtomicBool::new(false));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buf = vec![0u8; 65536];
                let n = match stream.read(&mut buf) {
                    Ok(n) if n > 0 => n,
                    _ => continue,
                };
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let line = request.lines().next().unwrap_or_default().to_string();
                let method = line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|p| p.rsplit('/').next())
                    .map(|m| m.split('?').next().unwrap_or(m).to_string())
                    .unwrap_or_default();
                let body = request
                    .find("\r\n\r\n")
                    .map(|i| request[i + 4..].to_string())
                    .unwrap_or_default();
                kept.lock()
                    .unwrap()
                    .push((method.clone(), format!("{line}\n{body}")));
                let reply = if method == "getUpdates" && !served.swap(true, Ordering::SeqCst) {
                    format!(
                        r#"{{"ok":true,"result":[{{"update_id":5,"callback_query":{{"id":"cb-1","from":{{"id":{chat},"username":"max"}},"message":{{"message_id":77,"chat":{{"id":{chat}}},"date":1700000000}},"data":"/connect notion"}}}}]}}"#
                    )
                } else if method == "getUpdates" {
                    r#"{"ok":true,"result":[]}"#.to_string()
                } else {
                    r#"{"ok":true,"result":true}"#.to_string()
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    reply.len(),
                    reply
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        (format!("http://127.0.0.1:{port}"), calls)
    }

    #[tokio::test]
    async fn a_button_tap_arrives_as_its_command_and_is_answered() {
        let (base_url, calls) = button_server(9999);
        let channel = Arc::new(
            TelegramChannel::with_base_url("TESTTOKEN", base_url).with_allowed_chats(vec![9999]),
        );
        let (tx, mut rx) = mpsc::channel(8);
        let run_channel = channel.clone();
        let handle = tokio::spawn(async move { run_channel.run(tx).await });
        let got = timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.chat_id, "9999");
        assert_eq!(got.text, "/connect notion");
        assert_eq!(got.sender, "max");
        assert!(got.message_id.is_empty(), "no owner message to react to");
        for _ in 0..100 {
            if calls
                .lock()
                .unwrap()
                .iter()
                .any(|(m, _)| m == "answerCallbackQuery")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        handle.abort();
        let calls = calls.lock().unwrap();
        let answer = calls
            .iter()
            .find(|(m, _)| m == "answerCallbackQuery")
            .expect("the tap is answered");
        assert!(answer.1.contains(r#""callback_query_id":"cb-1""#));
        let poll = calls.iter().find(|(m, _)| m == "getUpdates").unwrap();
        assert!(poll
            .1
            .contains("allowed_updates=%5B%22message%22%2C%22callback_query%22%5D"));
    }

    #[tokio::test]
    async fn a_tap_from_a_chat_that_isnt_allowed_goes_nowhere() {
        let (base_url, _calls) = button_server(1234);
        let channel = Arc::new(
            TelegramChannel::with_base_url("TESTTOKEN", base_url).with_allowed_chats(vec![9999]),
        );
        let (tx, mut rx) = mpsc::channel(8);
        let run_channel = channel.clone();
        let handle = tokio::spawn(async move { run_channel.run(tx).await });
        let got = timeout(Duration::from_millis(700), rx.recv()).await;
        handle.abort();
        assert!(got.is_err() || got.unwrap().is_none());
    }

    #[tokio::test]
    async fn buttons_go_out_as_an_inline_keyboard() {
        let (base_url, calls) = button_server(9999);
        let channel = TelegramChannel::with_base_url("TESTTOKEN", base_url);
        let out = OutboundMessage {
            channel: "telegram".into(),
            chat_id: "9999".into(),
            text: "Connect Notion?".into(),
            reply_to: None,
            attachments: vec![],
        };
        let buttons = vec![
            Button {
                text: "Connect".into(),
                action: ButtonAction::Url("https://relay.example/x".into()),
            },
            Button {
                text: "Decline".into(),
                action: ButtonAction::Command("/decline notion".into()),
            },
        ];
        crate::channel::send_with_buttons(&channel, out.clone(), &buttons)
            .await
            .unwrap();
        // Too long for callback_data: sent as text instead.
        let long = vec![Button {
            text: "Run".into(),
            action: ButtonAction::Command(format!("/connect {}", "x".repeat(80))),
        }];
        crate::channel::send_with_buttons(&channel, out, &long)
            .await
            .unwrap();
        let calls = calls.lock().unwrap();
        let sent: Vec<Value> = calls
            .iter()
            .filter(|(m, _)| m == "sendMessage")
            .map(|(_, b)| serde_json::from_str(b.split_once('\n').unwrap().1).unwrap())
            .collect();
        assert_eq!(sent.len(), 2);
        assert_eq!(
            sent[0]["reply_markup"],
            json!({"inline_keyboard": [
                [{"text": "Connect", "url": "https://relay.example/x"}],
                [{"text": "Decline", "callback_data": "/decline notion"}]
            ]})
        );
        assert!(sent[1].get("reply_markup").is_none());
        assert!(sent[1]["text"]
            .as_str()
            .unwrap()
            .contains("• Run: send /connect xxx"));
    }

    #[test]
    fn capabilities_report_edits_and_reactions() {
        let channel = TelegramChannel::new("t");
        let caps = channel.capabilities();
        assert!(caps.edits);
        assert!(caps.reactions);
        assert!(!caps.attachments);
        assert!(caps.buttons);
        assert!(channel.polls());
        assert_eq!(channel.last_ok_poll(), None);
    }

    /// What the scripted server does with the n-th `getUpdates` call.
    #[derive(Clone, Copy)]
    enum Poll {
        /// Read the request, then never answer — a half-open connection.
        Hang,
        /// An HTML error page with this status, like a proxy in trouble.
        Html(u16),
        /// Telegram's own not-ok envelope with this error code.
        NotOk(u16),
    }

    /// A Bot API server that plays `script` on the first `getUpdates` calls,
    /// then serves one update for chat 9999 and empty results after that.
    /// Returns the base URL and how many polls it has seen.
    fn scripted_server(script: Vec<Poll>) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::AtomicUsize;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let polls = Arc::new(AtomicUsize::new(0));
        let seen = polls.clone();
        std::thread::spawn(move || {
            let mut parked = Vec::new();
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buf = vec![0u8; 65536];
                let n = match stream.read(&mut buf) {
                    Ok(n) if n > 0 => n,
                    _ => continue,
                };
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                if !request.contains("getUpdates") {
                    continue;
                }
                let i = seen.fetch_add(1, Ordering::SeqCst);
                let (status, body) = match script.get(i) {
                    Some(Poll::Hang) => {
                        parked.push(stream);
                        continue;
                    }
                    Some(Poll::Html(code)) => (*code, "<html>bad gateway</html>".to_string()),
                    Some(Poll::NotOk(code)) => (
                        *code,
                        format!(r#"{{"ok":false,"error_code":{code},"description":"nope"}}"#),
                    ),
                    None if i == script.len() => (
                        200,
                        r#"{"ok":true,"result":[{"update_id":7,"message":{"message_id":1,"chat":{"id":9999},"from":{"username":"max"},"text":"still there?","date":1700000000}}]}"#.to_string(),
                    ),
                    None => (200, r#"{"ok":true,"result":[]}"#.to_string()),
                };
                let resp = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            }
        });
        (format!("http://127.0.0.1:{port}"), polls)
    }

    async fn first_message_after(script: Vec<Poll>) -> (Option<InboundMessage>, usize) {
        let (base_url, polls) = scripted_server(script);
        let channel = Arc::new(
            TelegramChannel::with_base_url("TESTTOKEN", base_url)
                .with_allowed_chats(vec![9999])
                .with_fast_retries(Duration::from_millis(300)),
        );
        let (tx, mut rx) = mpsc::channel(8);
        let run_channel = channel.clone();
        let handle = tokio::spawn(async move { run_channel.run(tx).await });
        let got = timeout(Duration::from_secs(5), rx.recv())
            .await
            .ok()
            .flatten();
        handle.abort();
        (got, polls.load(Ordering::SeqCst))
    }

    #[tokio::test]
    async fn a_poll_that_never_answers_times_out_and_the_bot_keeps_listening() {
        let (got, polls) = first_message_after(vec![Poll::Hang, Poll::Hang]).await;
        assert_eq!(
            got.expect("the message after two hung polls").text,
            "still there?"
        );
        assert!(polls >= 3);
    }

    #[tokio::test]
    async fn telegram_or_proxy_errors_are_retried_instead_of_stopping_the_bot() {
        let (got, _) = first_message_after(vec![
            Poll::Html(502),
            Poll::NotOk(500),
            Poll::NotOk(409),
            Poll::NotOk(429),
        ])
        .await;
        assert_eq!(
            got.expect("the message after four failed polls").text,
            "still there?"
        );
    }

    #[tokio::test]
    async fn a_rejected_token_stops_the_adapter_with_an_error() {
        let (base_url, polls) = scripted_server(vec![Poll::NotOk(401)]);
        let channel = TelegramChannel::with_base_url("BADTOKEN", base_url)
            .with_allowed_chats(vec![9999])
            .with_fast_retries(Duration::from_millis(300));
        let (tx, _rx) = mpsc::channel(8);
        let result = timeout(Duration::from_secs(5), channel.run(tx))
            .await
            .expect("a rejected token ends run() promptly");
        let err = result.expect_err("401 is fatal").to_string();
        assert!(err.contains("401"), "{err}");
        assert_eq!(
            polls.load(Ordering::SeqCst),
            1,
            "no retry on a rejected token"
        );
    }

    /// A Bot API server for M19c: plays `polls` on the first `getUpdates`
    /// calls (then empty results, a little slowly), keeps a webhook until
    /// `deleteWebhook`, and records every other call as (method, body).
    struct Bot {
        url: String,
        calls: Arc<Mutex<Vec<(String, Value)>>>,
        /// Every `setMyCommands` body (M48), kept apart from `calls`.
        menus: Arc<Mutex<Vec<Value>>>,
    }

    impl Bot {
        fn start(webhook: &str, polls: Vec<Value>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
            let calls: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
            let log = calls.clone();
            let menus: Arc<Mutex<Vec<Value>>> = Arc::default();
            let set_menu = menus.clone();
            let webhook = Arc::new(Mutex::new(webhook.to_string()));
            let polls = Arc::new(Mutex::new(std::collections::VecDeque::from(polls)));
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { break };
                    let mut buf = vec![0u8; 65536];
                    let n = match stream.read(&mut buf) {
                        Ok(n) if n > 0 => n,
                        _ => continue,
                    };
                    let request = String::from_utf8_lossy(&buf[..n]).to_string();
                    let method = request
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("")
                        .split('?')
                        .next()
                        .unwrap_or("")
                        .rsplit('/')
                        .next()
                        .unwrap_or("")
                        .to_string();
                    let body = request
                        .find("\r\n\r\n")
                        .and_then(|i| serde_json::from_str(&request[i + 4..]).ok())
                        .unwrap_or(Value::Null);
                    let (status, out) = match method.as_str() {
                        "getUpdates" => match polls.lock().unwrap().pop_front() {
                            // Another service sets a webhook: the poll is refused.
                            Some(mut v) if v.get("_webhook").is_some() => {
                                let hook = v["_webhook"].as_str().unwrap().to_string();
                                *webhook.lock().unwrap() = hook;
                                v.as_object_mut().unwrap().remove("_webhook");
                                (409, v)
                            }
                            Some(v) => (
                                v.get("error_code").and_then(Value::as_u64).unwrap_or(200),
                                v,
                            ),
                            None => {
                                std::thread::sleep(Duration::from_millis(30));
                                (200, json!({"ok": true, "result": []}))
                            }
                        },
                        "getWebhookInfo" => (
                            200,
                            json!({"ok": true, "result": {"url": *webhook.lock().unwrap()}}),
                        ),
                        "setMyCommands" => {
                            set_menu.lock().unwrap().push(body);
                            (200, json!({"ok": true, "result": true}))
                        }
                        _ => {
                            if method == "deleteWebhook" {
                                webhook.lock().unwrap().clear();
                            }
                            log.lock().unwrap().push((method, body));
                            (200, json!({"ok": true, "result": true}))
                        }
                    };
                    let out = out.to_string();
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{out}",
                        out.len()
                    );
                    let _ = stream.write_all(resp.as_bytes());
                }
            });
            Self { url, calls, menus }
        }

        /// The texts sent to `chat`.
        fn texts_to(&self, chat: &str) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(m, b)| m == "sendMessage" && b["chat_id"] == chat)
                .map(|(_, b)| b["text"].as_str().unwrap_or_default().to_string())
                .collect()
        }

        fn methods(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(m, _)| m.clone())
                .collect()
        }
    }

    fn updates(messages: Vec<Value>) -> Value {
        let result: Vec<Value> = messages
            .into_iter()
            .enumerate()
            .map(|(i, mut m)| {
                m["message_id"] = json!(i + 1);
                m["date"] = json!(1700000000);
                m["from"] = json!({"username": "max"});
                if m.get("chat").is_none() {
                    m["chat"] = json!({"id": 9999});
                }
                json!({"update_id": i + 1, "message": m})
            })
            .collect();
        json!({"ok": true, "result": result})
    }

    fn conflict(description: &str) -> Value {
        json!({"ok": false, "error_code": 409, "description": description})
    }

    const OTHER_POLLER: &str =
        "Conflict: terminated by other getUpdates request; make sure that only one bot instance is running";

    fn channel(bot: &Bot) -> Arc<TelegramChannel> {
        Arc::new(
            TelegramChannel::with_base_url("TESTTOKEN", &bot.url)
                .with_allowed_chats(vec![9999, 42])
                .with_owner(Some(42))
                .with_fast_retries(Duration::from_millis(500)),
        )
    }

    async fn until(what: &str, mut done: impl FnMut() -> bool) {
        for _ in 0..300 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}");
    }

    #[tokio::test]
    async fn a_webhook_is_removed_at_start_keeping_pending_updates_and_the_owner_is_told() {
        let bot = Bot::start("https://hooks.example.com/s3cret-path?token=abc", vec![]);
        let channel = channel(&bot);
        let (tx, _rx) = mpsc::channel(8);
        let run = channel.clone();
        let handle = tokio::spawn(async move { run.run(tx).await });
        until("the owner to hear about the webhook", || {
            !bot.texts_to("42").is_empty()
        })
        .await;
        handle.abort();
        let calls = bot.calls.lock().unwrap().clone();
        let (_, delete) = calls.iter().find(|(m, _)| m == "deleteWebhook").unwrap();
        assert_eq!(delete["drop_pending_updates"], false);
        let told = bot.texts_to("42");
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(
            told[0].contains("webhook set (to hooks.example.com)"),
            "{}",
            told[0]
        );
        assert!(told[0].contains("were kept"), "{}", told[0]);
        assert!(
            !told[0].contains("s3cret"),
            "a webhook's path stays private"
        );
    }

    #[tokio::test]
    async fn no_webhook_means_nothing_is_deleted_and_nobody_is_told() {
        let bot = Bot::start("", vec![updates(vec![json!({"text": "hi"})])]);
        let channel = channel(&bot);
        let (tx, mut rx) = mpsc::channel(8);
        let run = channel.clone();
        let handle = tokio::spawn(async move { run.run(tx).await });
        let got = timeout(Duration::from_secs(3), rx.recv()).await.unwrap();
        handle.abort();
        assert_eq!(got.unwrap().text, "hi");
        assert!(bot.methods().is_empty(), "{:?}", bot.methods());
    }

    /// The gateway's three, `stop` for everyone, and sixteen for the owner.
    fn full_menu() -> Vec<crate::menu::Command> {
        use crate::menu::{Command, Shown, BUILT_IN};
        const OWNER: [&str; 16] = [
            "resume",
            "plan",
            "undo",
            "model",
            "update",
            "restart",
            "doctor",
            "login",
            "logout",
            "connect",
            "connections",
            "skills",
            "mcp",
            "hooks",
            "caps",
            "dashboard",
        ];
        let one = |name: &'static str, shown| Command {
            name,
            args: "",
            menu: "A command",
            help: "a command",
            shown,
        };
        let mut v = BUILT_IN.to_vec();
        v.push(one("stop", Shown::Everywhere));
        v.extend(OWNER.iter().map(|n| one(n, Shown::Private)));
        v
    }

    fn names(body: &Value) -> Vec<&str> {
        body["commands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["command"].as_str().unwrap())
            .collect()
    }

    #[tokio::test]
    async fn the_menu_is_registered_per_scope() {
        let bot = Bot::start("", vec![updates(vec![json!({"text": "hi"})])]);
        let channel = Arc::new(
            TelegramChannel::with_base_url("TESTTOKEN", &bot.url)
                .with_allowed_chats(vec![9999, 42])
                .with_owner(Some(42))
                .with_menu(full_menu())
                .with_fast_retries(Duration::from_millis(500)),
        );
        let (tx, mut rx) = mpsc::channel(8);
        let run = channel.clone();
        let handle = tokio::spawn(async move { run.run(tx).await });
        timeout(Duration::from_secs(3), rx.recv()).await.unwrap();
        until("four menus", || bot.menus.lock().unwrap().len() == 4).await;
        handle.abort();
        let menus = bot.menus.lock().unwrap().clone();
        let kinds: Vec<&str> = menus
            .iter()
            .map(|m| m["scope"]["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            ["default", "all_group_chats", "all_private_chats", "chat"]
        );
        assert_eq!(names(&menus[0]), ["new", "status", "help", "stop"]);
        assert_eq!(names(&menus[2]).len(), 20);
        assert_eq!(names(&menus[3]).len(), 20);
        assert_eq!(menus[3]["scope"]["chat_id"], 42);
    }

    #[tokio::test]
    async fn groups_get_the_short_list() {
        let bot = Bot::start("", vec![updates(vec![json!({"text": "hi"})])]);
        let channel = channel(&bot);
        let (tx, mut rx) = mpsc::channel(8);
        let run = channel.clone();
        let handle = tokio::spawn(async move { run.run(tx).await });
        timeout(Duration::from_secs(3), rx.recv()).await.unwrap();
        until("the menus", || bot.menus.lock().unwrap().len() == 4).await;
        handle.abort();
        let menus = bot.menus.lock().unwrap().clone();
        let group = menus
            .iter()
            .find(|m| m["scope"]["type"] == "all_group_chats")
            .unwrap();
        assert_eq!(names(group), ["new", "status", "help"]);
    }

    #[tokio::test]
    async fn a_409_naming_a_webhook_removes_it_without_waiting() {
        let mut refused = conflict(
            "Conflict: can't use getUpdates method while webhook is active; use deleteWebhook to delete the webhook first",
        );
        // Set after the start-up check, as another service would.
        refused["_webhook"] = json!("https://other.example.net/hook");
        let bot = Bot::start("", vec![refused, updates(vec![json!({"text": "back"})])]);
        let channel = channel(&bot);
        let (tx, mut rx) = mpsc::channel(8);
        let run = channel.clone();
        let handle = tokio::spawn(async move { run.run(tx).await });
        let got = timeout(Duration::from_secs(3), rx.recv()).await.unwrap();
        handle.abort();
        assert_eq!(got.unwrap().text, "back");
        assert!(bot.methods().contains(&"deleteWebhook".to_string()));
        let told = bot.texts_to("42");
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(told[0].contains("other.example.net"), "{}", told[0]);
        assert_eq!(
            channel.problem(),
            None,
            "no conflict episode for a removed webhook"
        );
    }

    #[tokio::test]
    async fn a_lasting_409_is_told_to_the_owner_once_and_so_is_its_end() {
        let bot = Bot::start("", (0..8).map(|_| conflict(OTHER_POLLER)).collect());
        let channel = Arc::new(
            TelegramChannel::with_base_url("TESTTOKEN", &bot.url)
                .with_allowed_chats(vec![9999, 42])
                .with_owner(Some(42))
                .with_conflict_after(Duration::from_millis(150))
                .with_fast_retries(Duration::from_millis(500)),
        );
        let (tx, _rx) = mpsc::channel(8);
        let run = channel.clone();
        let handle = tokio::spawn(async move { run.run(tx).await });
        until("the conflict notice", || !bot.texts_to("42").is_empty()).await;
        let problem = channel.problem().expect("/status shows the conflict");
        assert!(problem.contains("409 Conflict"), "{problem}");
        assert!(problem.contains("the owner was told"), "{problem}");
        until("the all-clear", || bot.texts_to("42").len() >= 2).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        handle.abort();
        let told = bot.texts_to("42");
        assert_eq!(told.len(), 2, "one notice, one all-clear: {told:?}");
        assert!(told[0].contains("409 Conflict"), "{}", told[0]);
        assert!(
            told[0].contains("another program is fetching this bot's messages with the same token"),
            "{}",
            told[0]
        );
        assert!(
            told[0].contains("webhook"),
            "both causes are named: {}",
            told[0]
        );
        assert!(told[1].contains("messages again"), "{}", told[1]);
        assert_eq!(channel.problem(), None);
    }

    #[tokio::test]
    async fn a_short_409_blip_tells_nobody() {
        let bot = Bot::start("", vec![conflict(OTHER_POLLER), conflict(OTHER_POLLER)]);
        let channel = channel(&bot);
        let (tx, _rx) = mpsc::channel(8);
        let run = channel.clone();
        let handle = tokio::spawn(async move { run.run(tx).await });
        tokio::time::sleep(Duration::from_millis(400)).await;
        handle.abort();
        assert!(bot.texts_to("42").is_empty(), "{:?}", bot.texts_to("42"));
    }

    #[tokio::test]
    async fn a_caption_reaches_the_agent_and_a_message_with_no_text_gets_a_plain_reply() {
        let bot = Bot::start(
            "",
            vec![updates(vec![
                json!({"voice": {"file_id": "v", "duration": 3}}),
                json!({"photo": [{"file_id": "p"}], "caption": "what is this?"}),
                json!({"photo": [{"file_id": "a1"}], "media_group_id": "g1"}),
                json!({"photo": [{"file_id": "a2"}], "media_group_id": "g1"}),
                json!({"animation": {"file_id": "x"}, "document": {"file_id": "x"}}),
                json!({"new_chat_members": [{"id": 5}]}),
                json!({"chat": {"id": 777}, "sticker": {"file_id": "s"}}),
                json!({"text": "plain text"}),
            ])],
        );
        let channel = channel(&bot);
        let (tx, mut rx) = mpsc::channel(8);
        let run = channel.clone();
        let handle = tokio::spawn(async move { run.run(tx).await });
        let first = timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let second = timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        handle.abort();
        assert_eq!(
            first.text,
            "what is this?\n\n[The photo attached to this message wasn't read: ferrule reads only text for now.]"
        );
        assert_eq!(second.text, "plain text");
        let told = bot.texts_to("9999");
        assert_eq!(told.len(), 3, "voice, one for the album, the GIF: {told:?}");
        assert!(
            told[0].starts_with("I got your voice message, but I can only read text for now"),
            "{}",
            told[0]
        );
        assert!(told[1].starts_with("I got your photo,"), "{}", told[1]);
        assert!(told[2].starts_with("I got your GIF,"), "{}", told[2]);
        let calls = bot.calls.lock().unwrap().clone();
        assert_eq!(
            calls[0].1["reply_to_message_id"], 1,
            "it answers the voice message"
        );
        assert!(
            bot.texts_to("777").is_empty(),
            "a stranger's sticker gets silence"
        );
    }

    thread_local! {
        static SINK: std::cell::RefCell<Option<Arc<Mutex<Vec<String>>>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Clears this thread's log sink when the test ends.
    struct Logged;
    impl Drop for Logged {
        fn drop(&mut self) {
            SINK.with(|s| s.borrow_mut().take());
        }
    }

    /// Every event's level, message and fields logged on this thread (a
    /// `#[tokio::test]` runtime and its spawned tasks run on the test's
    /// thread) until the returned guard drops. One global subscriber for
    /// the whole test binary, not a scoped `set_default` per test: scoped
    /// dispatchers come and go while other tests register callsites, and
    /// tracing's global interest cache then drops events at random (seen
    /// on windows-latest and locally with 16 test threads).
    fn logged() -> (Arc<Mutex<Vec<String>>>, Logged) {
        use tracing_subscriber::layer::SubscriberExt;
        struct Capture;
        struct Text(String);
        impl tracing::field::Visit for Text {
            fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                self.0.push_str(&format!(" {}={v:?}", f.name()));
            }
        }
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let Some(sink) = SINK.with(|s| s.borrow().clone()) else {
                    return;
                };
                let mut text = Text(event.metadata().level().to_string());
                event.record(&mut text);
                sink.lock().unwrap().push(text.0);
            }
        }
        static INSTALL: std::sync::Once = std::sync::Once::new();
        INSTALL.call_once(|| {
            tracing::subscriber::set_global_default(tracing_subscriber::registry().with(Capture))
                .expect("the only global subscriber in this test binary");
        });
        // A callsite registering on another thread while the subscriber
        // was being installed may have cached "never"; recompute.
        tracing::callsite::rebuild_interest_cache();
        let lines: Arc<Mutex<Vec<String>>> = Arc::default();
        SINK.with(|s| *s.borrow_mut() = Some(lines.clone()));
        (lines, Logged)
    }

    #[tokio::test]
    async fn an_ignored_chat_is_a_warning_naming_it_once_an_hour() {
        let (lines, _guard) = logged();
        let bot = Bot::start("", vec![]);
        let channel = channel(&bot);
        let msg = |chat: &str| InboundMessage {
            channel: "telegram".into(),
            chat_id: chat.into(),
            sender: "x".into(),
            sender_id: None,
            message_id: "1".into(),
            text: "hi".into(),
            attachments: vec![],
            reply_to: None,
            ts: 0,
        };
        for chat in ["5", "5", "6", "5"] {
            assert!(!channel.admits(&msg(chat)).await);
        }
        let warns: Vec<String> = lines
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.starts_with("WARN"))
            .cloned()
            .collect();
        assert_eq!(warns.len(), 2, "{warns:?}");
        assert!(
            warns[0].contains("chat 5") && warns[0].contains("telegram_allowed_chats"),
            "{}",
            warns[0]
        );
        assert!(warns[1].contains("chat 6"), "{}", warns[1]);
        assert!(bot.texts_to("5").is_empty());
    }

    #[tokio::test]
    async fn no_log_line_or_error_carries_the_bot_token() {
        let (lines, _guard) = logged();
        // Nothing listens here: every call fails at connect, the kind of
        // reqwest error whose Display would include the URL.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let channel = Arc::new(
            TelegramChannel::with_base_url(
                "123456:SECRET-TOKEN",
                format!("http://127.0.0.1:{port}"),
            )
            .with_allowed_chats(vec![42])
            .with_owner(Some(42))
            .with_fast_retries(Duration::from_millis(200)),
        );
        let (tx, _rx) = mpsc::channel(8);
        let run = channel.clone();
        let handle = tokio::spawn(async move { run.run(tx).await });
        // Windows takes about 2 s to refuse a connection to a closed port.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !lines
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("poll failed"))
        {
            assert!(
                std::time::Instant::now() < deadline,
                "no poll failed: {:?}",
                lines.lock().unwrap()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        handle.abort();
        let out = OutboundMessage {
            channel: "telegram".into(),
            chat_id: "42".into(),
            text: "x".into(),
            reply_to: None,
            attachments: vec![],
        };
        let errors = [
            channel.send(out).await.unwrap_err().to_string(),
            channel
                .react("42", "1", "👀")
                .await
                .unwrap_err()
                .to_string(),
            channel.edit("42", "1", "x").await.unwrap_err().to_string(),
        ];
        let lines = lines.lock().unwrap().clone();
        assert!(lines.iter().any(|l| l.contains("poll failed")), "{lines:?}");
        for line in lines.iter().chain(errors.iter()) {
            assert!(!line.contains("SECRET-TOKEN"), "the token leaked: {line}");
        }
    }
}
