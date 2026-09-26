//! Discord adapter (M31): the Gateway WebSocket for what arrives, the REST
//! API for what goes out. `api` is a constructor argument so tests point it
//! at a mock on 127.0.0.1; the socket's address comes from the API
//! (`GET /gateway/bot`), so the mock names its own.
//!
//! Who gets in (`access`): a DM from an allowed user; in a guild, a message
//! in an allowed channel (or a thread under one) that mentions the bot or
//! replies to it. A DM's chat is its author's id, so the owner is one id
//! that says both who and where; a guild chat is the channel's id.
//!
//! What goes out is plain Markdown, which Discord renders, split at 2000
//! characters, with pings turned off. Buttons are components: a tap is
//! acknowledged at once (Discord wants an answer in 3 s) and arrives as the
//! command it carries. A slash command is deferred ("thinking…") and the
//! next reply to its chat becomes the answer.

mod gateway;
pub mod ratelimit;

use crate::channel::{Button, ButtonAction, Channel, ChannelCapabilities};
use crate::channels::access::{self, Access, Dm};
use crate::error::GatewayError;
use crate::message::{InboundMessage, OutboundMessage};
use crate::stream::chunks;
use ratelimit::Limits;
use reqwest::Method;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::mpsc;

/// Discord's REST API, v10.
pub const API_URL: &str = "https://discord.com/api/v10";

/// Discord's cap on one message.
pub const MESSAGE_LIMIT: usize = 2000;

/// GUILDS | GUILD_MESSAGES | DIRECT_MESSAGES.
pub const INTENTS_BASE: u64 = 1 | (1 << 9) | (1 << 12);
/// MESSAGE_CONTENT: privileged, switched on in the developer portal.
pub const INTENT_MESSAGE_CONTENT: u64 = 1 << 15;

/// The application flags that say MESSAGE_CONTENT is on (verified apps,
/// and apps in under 100 servers).
const FLAG_CONTENT: u64 = (1 << 18) | (1 << 19);

/// The slash commands `ferrule setup` registers, each with an optional
/// `args` string.
pub const SLASH_COMMANDS: [(&str, &str); 11] = [
    ("status", "What ferrule is doing"),
    ("stop", "Stop every run (kill switch)"),
    ("resume", "Let runs start again"),
    ("model", "Show or switch the model"),
    ("dashboard", "A link to the dashboard"),
    ("skills", "The installed skills"),
    ("undo", "Revert the agent's last commit"),
    ("plan", "Plan a task read-only, then ask"),
    ("caps", "Spending caps"),
    ("mcp", "MCP servers"),
    ("connections", "Connected services"),
];

/// View Channels, Send Messages, Send Messages in Threads, Read Message
/// History, Add Reactions.
pub const INVITE_PERMISSIONS: u64 = 274_877_975_552;

const REQUEST_DEADLINE: Duration = Duration::from_secs(30);
const CONNECT_DEADLINE: Duration = Duration::from_secs(10);
/// How long a deferred slash command waits for its answer.
const SLASH_WINDOW: Duration = Duration::from_secs(15 * 60);

/// Waits and retries, shortened in tests.
#[derive(Clone)]
struct Timing {
    backoff_min: Duration,
    backoff_max: Duration,
    /// After INVALID_SESSION, a random wait in this range.
    invalid_session: (Duration, Duration),
    hello: Duration,
    connect: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            invalid_session: (Duration::from_secs(1), Duration::from_secs(5)),
            hello: Duration::from_secs(20),
            connect: Duration::from_secs(20),
        }
    }
}

/// The resumable gateway session.
#[derive(Default)]
struct Session {
    id: Option<String>,
    resume_url: Option<String>,
    seq: Option<u64>,
}

/// A slash command waiting for its answer.
struct Deferred {
    app: String,
    token: String,
    at: Instant,
}

pub struct DiscordChannel {
    api: String,
    token: String,
    client: reqwest::Client,
    limits: Limits,
    access: Access,
    timing: Timing,
    session: Mutex<Session>,
    /// The bot's user id and application id, from READY.
    bot: Mutex<Option<String>>,
    app: Mutex<Option<String>>,
    /// Whether to ask for MESSAGE_CONTENT (off after a 4014).
    content_intent: AtomicBool,
    /// User → DM channel, and back.
    dm_channel: Mutex<HashMap<String, String>>,
    dm_user: Mutex<HashMap<String, String>>,
    /// Guild channels seen: id → the parent, for a thread.
    guild_channels: Mutex<HashMap<String, Option<String>>>,
    /// Chat → the slash command its next reply answers.
    deferred: Mutex<HashMap<String, Deferred>>,
    /// Streamed answers to slash commands: our id → the interaction.
    hooks: Mutex<HashMap<String, (String, String)>>,
    hook_ids: AtomicU64,
    last_frame: Mutex<Option<SystemTime>>,
    /// The socket's trouble, and the lost intent (which outlasts it).
    problem: Mutex<Option<String>>,
    intent_note: Mutex<Option<String>>,
}

impl DiscordChannel {
    /// Real usage: talks to `https://discord.com/api/v10`.
    pub fn new(token: impl Into<String>) -> Self {
        Self::with_api(token, API_URL)
    }

    /// Tests: an arbitrary API base (a mock on 127.0.0.1).
    pub fn with_api(token: impl Into<String>, api: impl Into<String>) -> Self {
        let api = api.into().trim_end_matches('/').to_string();
        Self {
            api: api.clone(),
            token: token.into(),
            client: client(&api),
            limits: Limits::default(),
            access: Access::new("discord", "Discord user id", vec![], vec![]),
            timing: Timing::default(),
            session: Mutex::new(Session::default()),
            bot: Mutex::new(None),
            app: Mutex::new(None),
            content_intent: AtomicBool::new(true),
            dm_channel: Mutex::new(HashMap::new()),
            dm_user: Mutex::new(HashMap::new()),
            guild_channels: Mutex::new(HashMap::new()),
            deferred: Mutex::new(HashMap::new()),
            hooks: Mutex::new(HashMap::new()),
            hook_ids: AtomicU64::new(0),
            last_frame: Mutex::new(None),
            problem: Mutex::new(None),
            intent_note: Mutex::new(None),
        }
    }

    /// Who may talk to the bot: users in DMs, channels (and their
    /// threads) when the bot is addressed.
    pub fn with_allowed(mut self, users: Vec<String>, channels: Vec<String>) -> Self {
        self.access = Access::new("discord", "Discord user id", users, channels);
        self
    }

    /// Setup only: the first DM that is exactly `code` pairs its author.
    pub fn with_pairing(mut self, code: impl Into<String>) -> Self {
        let access = std::mem::replace(
            &mut self.access,
            Access::new("discord", "Discord user id", vec![], vec![]),
        );
        self.access = access.with_pairing(code);
        self
    }

    /// Who paired during setup, `(user id, name)`.
    pub fn paired(&self) -> Option<(String, String)> {
        self.access.paired()
    }

    /// Tests: waits in milliseconds, not seconds.
    #[doc(hidden)]
    pub fn with_fast_retries(mut self) -> Self {
        self.timing = Timing {
            backoff_min: Duration::from_millis(20),
            backoff_max: Duration::from_millis(100),
            invalid_session: (Duration::from_millis(10), Duration::from_millis(30)),
            hello: Duration::from_secs(5),
            connect: Duration::from_secs(5),
        };
        self
    }

    fn frame(&self) {
        *self.last_frame.lock().unwrap() = Some(SystemTime::now());
    }

    fn set_problem(&self, p: Option<String>) {
        *self.problem.lock().unwrap() = p;
    }

    fn bot_id(&self) -> Option<String> {
        self.bot.lock().unwrap().clone()
    }

    /// One REST call; `Err` says why in words, never with the URL or token.
    /// `patient`: wait out a full bucket (≤ 60 s) and retry a 429 up to
    /// three times; otherwise a limit is `RateLimited` at once.
    async fn rest(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        patient: bool,
    ) -> Result<Value, GatewayError> {
        let route = ratelimit::route(method.as_str(), path);
        let what = format!("{} {}", method, route_words(&route));
        let mut tries = 0;
        loop {
            tries += 1;
            let wait = self.limits.wait_for(&route);
            if !wait.is_zero() {
                if !patient || wait > ratelimit::MAX_WAIT {
                    return Err(GatewayError::RateLimited { retry_after: wait });
                }
                tokio::time::sleep(wait).await;
            }
            let mut req = self
                .client
                .request(method.clone(), format!("{}{path}", self.api));
            // Interaction callbacks and webhooks carry their own token.
            if !path.starts_with("/interactions/") && !path.starts_with("/webhooks/") {
                req = req.header("Authorization", format!("Bot {}", self.token));
            }
            if let Some(b) = &body {
                req = req.json(b);
            }
            let resp = req.send().await.map_err(|e| {
                GatewayError::Channel(format!("discord {what} failed: {}", e.without_url()))
            })?;
            let status = resp.status();
            self.limits.update(&route, resp.headers());
            let global_scope = resp
                .headers()
                .get("x-ratelimit-scope")
                .and_then(|v| v.to_str().ok())
                == Some("global");
            let text = resp.text().await.unwrap_or_default();
            let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            if status.as_u16() == 429 {
                let retry = ratelimit::secs(json["retry_after"].as_f64().unwrap_or(1.0));
                let global = json["global"].as_bool().unwrap_or(false) || global_scope;
                self.limits.limited(&route, retry, global);
                tracing::warn!(
                    global,
                    retry_after_ms = retry.as_millis() as u64,
                    "discord: rate limited on {what}"
                );
                if patient && tries < 3 && retry <= ratelimit::MAX_WAIT {
                    continue;
                }
                return Err(GatewayError::RateLimited { retry_after: retry });
            }
            if status.is_success() {
                return Ok(json);
            }
            return Err(GatewayError::Channel(format!(
                "discord {what} failed (status {status}): {}",
                crate::health::clip(&text, 300)
            )));
        }
    }

    /// Where a chat's messages go: a DM is keyed by its user, so the DM
    /// channel is looked up (or opened); a guild chat is its channel.
    async fn channel_of(&self, chat: &str) -> Result<(String, bool), GatewayError> {
        if self.guild_channels.lock().unwrap().contains_key(chat)
            || self.access.channel_allowed(chat)
        {
            return Ok((chat.to_string(), true));
        }
        if let Some(ch) = self.dm_channel.lock().unwrap().get(chat).cloned() {
            return Ok((ch, false));
        }
        let dm = self
            .rest(
                Method::POST,
                "/users/@me/channels",
                Some(json!({ "recipient_id": chat })),
                true,
            )
            .await?;
        let ch = dm["id"]
            .as_str()
            .ok_or_else(|| GatewayError::Channel("discord: opening a DM gave no channel".into()))?
            .to_string();
        self.remember_dm(chat, &ch);
        Ok((ch, false))
    }

    fn remember_dm(&self, user: &str, channel: &str) {
        self.dm_channel
            .lock()
            .unwrap()
            .insert(user.to_string(), channel.to_string());
        self.dm_user
            .lock()
            .unwrap()
            .insert(channel.to_string(), user.to_string());
    }

    /// A thread's parent channel, from the cache or `GET /channels/{id}`.
    async fn parent_of(&self, channel: &str) -> Option<String> {
        if let Some(p) = self.guild_channels.lock().unwrap().get(channel) {
            return p.clone();
        }
        let parent = match self
            .rest(Method::GET, &format!("/channels/{channel}"), None, true)
            .await
        {
            Ok(c) if is_thread(&c) => c["parent_id"].as_str().map(str::to_string),
            Ok(_) => None,
            Err(e) => {
                tracing::warn!(error = %e, "discord: couldn't look up a channel's parent");
                return None;
            }
        };
        self.guild_channels
            .lock()
            .unwrap()
            .insert(channel.to_string(), parent.clone());
        parent
    }

    fn learn_channel(&self, c: &Value) {
        let Some(id) = c["id"].as_str() else { return };
        let parent = is_thread(c)
            .then(|| c["parent_id"].as_str().map(str::to_string))
            .flatten();
        self.guild_channels
            .lock()
            .unwrap()
            .insert(id.to_string(), parent);
    }

    /// Whether a guild channel (or its thread's parent) is allowed.
    async fn guild_allowed(&self, channel: &str) -> bool {
        if self.access.channel_allowed(channel) {
            self.guild_channels
                .lock()
                .unwrap()
                .entry(channel.to_string())
                .or_insert(None);
            return true;
        }
        match self.parent_of(channel).await {
            Some(p) => self.access.channel_allowed(&p),
            None => false,
        }
    }

    /// A message into a channel id; the new message's id.
    async fn create(&self, channel: &str, body: Value) -> Result<Option<String>, GatewayError> {
        let m = self
            .rest(
                Method::POST,
                &format!("/channels/{channel}/messages"),
                Some(body),
                true,
            )
            .await?;
        Ok(m["id"].as_str().map(str::to_string))
    }

    /// Sends `msg`, split at 2000; the last message's id. The first piece
    /// answers a waiting slash command, or replies to the message that
    /// asked (in a guild).
    async fn deliver(
        &self,
        msg: &OutboundMessage,
        components: Option<Value>,
    ) -> Result<Option<String>, GatewayError> {
        let (channel, guild) = self.channel_of(&msg.chat_id).await?;
        let mut pieces = chunks(&msg.text, MESSAGE_LIMIT);
        pieces.retain(|p| !p.trim().is_empty());
        if pieces.is_empty() {
            pieces.push("…".into());
        }
        let last = pieces.len() - 1;
        let mut id = None;
        for (i, piece) in pieces.into_iter().enumerate() {
            let mut body = json!({ "content": piece, "allowed_mentions": { "parse": [] } });
            if i == last {
                if let Some(c) = &components {
                    body["components"] = c.clone();
                }
            }
            if i == 0 {
                if let Some(d) = self.take_deferred(&msg.chat_id) {
                    self.rest(
                        Method::PATCH,
                        &format!("/webhooks/{}/{}/messages/@original", d.app, d.token),
                        Some(body),
                        true,
                    )
                    .await?;
                    let key = format!("hook:{}", self.hook_ids.fetch_add(1, Ordering::SeqCst));
                    self.hooks
                        .lock()
                        .unwrap()
                        .insert(key.clone(), (d.app, d.token));
                    id = Some(key);
                    continue;
                }
                let reply_to = msg.reply_to.as_deref().filter(|r| !r.is_empty());
                if let (true, Some(r)) = (guild, reply_to) {
                    body["message_reference"] =
                        json!({ "message_id": r, "fail_if_not_exists": false });
                }
            }
            id = self.create(&channel, body).await?;
        }
        Ok(id)
    }

    fn take_deferred(&self, chat: &str) -> Option<Deferred> {
        let mut deferred = self.deferred.lock().unwrap();
        deferred.retain(|_, d| d.at.elapsed() < SLASH_WINDOW);
        deferred.remove(chat)
    }

    /// Something to say in a channel id outside a turn (a stranger's id,
    /// the pairing answer, "I can only read text").
    async fn say(&self, channel: &str, text: String, reply_to: Option<&str>) {
        let mut body = json!({ "content": text, "allowed_mentions": { "parse": [] } });
        if let Some(r) = reply_to {
            body["message_reference"] = json!({ "message_id": r, "fail_if_not_exists": false });
        }
        if let Err(e) = self.create(channel, body).await {
            tracing::warn!(error = %e, "discord: couldn't answer a message");
        }
    }

    /// Answers an interaction (type 7: update the message; 5: deferred; 4:
    /// a message), within Discord's 3 s.
    async fn callback(&self, id: &str, token: &str, body: Value) {
        if let Err(e) = self
            .rest(
                Method::POST,
                &format!("/interactions/{id}/{token}/callback"),
                Some(body),
                false,
            )
            .await
        {
            tracing::warn!(error = %e, "discord: couldn't acknowledge an interaction");
        }
    }

    /// A `MESSAGE_CREATE`, if it's for the agent.
    async fn on_message(&self, d: &Value) -> Option<InboundMessage> {
        let author = &d["author"];
        let user = author["id"].as_str()?.to_string();
        if author["bot"].as_bool().unwrap_or(false)
            || d["webhook_id"].is_string()
            || Some(&user) == self.bot_id().as_ref()
        {
            return None;
        }
        let sender = name_of(author);
        let channel = d["channel_id"].as_str()?.to_string();
        let message_id = d["id"].as_str()?.to_string();
        let mut text = d["content"].as_str().unwrap_or("").to_string();
        let unread = unread_kind(d);
        let chat_id = if d["guild_id"].is_string() {
            let bot = self.bot_id()?;
            let mentioned = d["mentions"]
                .as_array()
                .is_some_and(|m| m.iter().any(|u| u["id"].as_str() == Some(&bot)));
            let replied = d["referenced_message"]["author"]["id"].as_str() == Some(&bot);
            if !mentioned && !replied {
                return None;
            }
            if !self.guild_allowed(&channel).await {
                self.access.ignore(&channel, &sender, "channel message");
                return None;
            }
            text = access::strip_mention(&text, &bot);
            channel.clone()
        } else {
            self.remember_dm(&user, &channel);
            match self.access.dm(&user, &sender, &text) {
                Dm::Admit => {}
                Dm::Tell(t) | Dm::Paired(t) => {
                    self.say(&channel, t, None).await;
                    return None;
                }
                Dm::Drop => return None,
            }
            user.clone()
        };
        if text.trim().is_empty() {
            if let Some(kind) = unread {
                tracing::info!(chat = %chat_id, "discord: got a {kind} with no text; ferrule reads only text");
                self.say(&channel, access::cannot_read_text(kind), Some(&message_id))
                    .await;
            }
            return None;
        }
        if let Some(kind) = unread {
            text = access::unread_note(&text, kind);
        }
        let ts = d["timestamp"]
            .as_str()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map_or(0, |t| t.timestamp());
        Some(InboundMessage {
            channel: "discord".into(),
            chat_id,
            sender,
            sender_id: Some(user),
            message_id,
            text,
            attachments: vec![],
            reply_to: None,
            ts,
        })
    }

    /// An `INTERACTION_CREATE`: a button tap (3) or a slash command (2).
    /// Acknowledged before anything else, then passed on as the text it
    /// stands for.
    async fn on_interaction(&self, d: &Value) -> Option<InboundMessage> {
        let kind = d["type"].as_u64()?;
        let (id, token) = (d["id"].as_str()?, d["token"].as_str()?);
        let channel = d["channel_id"].as_str()?.to_string();
        let who = if d["member"]["user"].is_object() {
            &d["member"]["user"]
        } else {
            &d["user"]
        };
        let user = who["id"].as_str()?.to_string();
        let sender = name_of(who);
        let guild = d["guild_id"].is_string();
        if !guild {
            self.remember_dm(&user, &channel);
        }
        let admitted = if guild {
            self.guild_allowed(&channel).await
        } else {
            self.access.user_allowed(&user)
        };
        let chat_id = if guild { channel.clone() } else { user.clone() };
        let text = match kind {
            3 => {
                let custom = d["data"]["custom_id"].as_str().unwrap_or("");
                if !admitted {
                    // Acknowledged without a change: a stranger's tap does nothing.
                    self.callback(id, token, json!({ "type": 6 })).await;
                    self.access.ignore(&chat_id, &sender, "button tap");
                    return None;
                }
                let label = button_label(&d["message"], custom).unwrap_or("done");
                let content = format!(
                    "{}\n→ {label}",
                    d["message"]["content"].as_str().unwrap_or("")
                );
                self.callback(
                    id,
                    token,
                    json!({ "type": 7, "data": { "content": clip_units(&content, MESSAGE_LIMIT), "components": [] } }),
                )
                .await;
                custom.strip_prefix("cmd:")?.to_string()
            }
            2 => {
                if !admitted {
                    self.callback(
                        id,
                        token,
                        json!({ "type": 4, "data": { "content": "This bot is private.", "flags": 64 } }),
                    )
                    .await;
                    self.access.ignore(&chat_id, &sender, "slash command");
                    return None;
                }
                self.callback(id, token, json!({ "type": 5 })).await;
                let name = d["data"]["name"].as_str()?;
                let args = d["data"]["options"]
                    .as_array()
                    .and_then(|o| o.iter().find(|o| o["name"] == "args"))
                    .and_then(|o| o["value"].as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let app = d["application_id"]
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| self.app.lock().unwrap().clone())?;
                self.deferred.lock().unwrap().insert(
                    chat_id.clone(),
                    Deferred {
                        app,
                        token: token.to_string(),
                        at: Instant::now(),
                    },
                );
                if args.is_empty() {
                    format!("/{name}")
                } else {
                    format!("/{name} {args}")
                }
            }
            _ => return None,
        };
        Some(InboundMessage {
            channel: "discord".into(),
            chat_id,
            sender,
            sender_id: Some(user),
            // No message of the user's to react to or reply to.
            message_id: String::new(),
            text,
            attachments: vec![],
            reply_to: None,
            ts: chrono::Utc::now().timestamp(),
        })
    }

    /// Components: action rows of up to five buttons.
    fn components(buttons: &[Button]) -> Result<Value, GatewayError> {
        let buttons = buttons
            .iter()
            .map(|b| {
                let label = clip_units(&b.text, 80);
                match &b.action {
                    ButtonAction::Url(url) => {
                        Ok(json!({ "type": 2, "style": 5, "label": label, "url": url }))
                    }
                    ButtonAction::Command(cmd) if cmd.len() + 4 <= 100 => Ok(
                        json!({ "type": 2, "style": 1, "label": label, "custom_id": format!("cmd:{cmd}") }),
                    ),
                    ButtonAction::Command(_) => Err(GatewayError::Unsupported(
                        "a button command longer than 96 bytes",
                    )),
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        if buttons.len() > 25 {
            return Err(GatewayError::Unsupported("more than 25 buttons"));
        }
        Ok(Value::Array(
            buttons
                .chunks(5)
                .map(|row| json!({ "type": 1, "components": row }))
                .collect(),
        ))
    }
}

#[async_trait::async_trait]
impl Channel for DiscordChannel {
    fn name(&self) -> &str {
        "discord"
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

    /// The last frame from the gateway: Discord ACKs a heartbeat every
    /// ~41 s, so a quiet but healthy socket stays fresh.
    fn last_ok_poll(&self) -> Option<SystemTime> {
        *self.last_frame.lock().unwrap()
    }

    fn problem(&self) -> Option<String> {
        let parts: Vec<String> = [
            self.problem.lock().unwrap().clone(),
            self.intent_note.lock().unwrap().clone(),
        ]
        .into_iter()
        .flatten()
        .collect();
        (!parts.is_empty()).then(|| parts.join("; "))
    }

    fn message_limit(&self) -> Option<usize> {
        Some(MESSAGE_LIMIT)
    }

    async fn run(&self, tx: mpsc::Sender<InboundMessage>) -> Result<(), GatewayError> {
        self.run_gateway(tx).await
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
        let components = Self::components(buttons)?;
        self.deliver(&msg, Some(components)).await.map(|_| ())
    }

    /// The 👀 receipt.
    async fn react(
        &self,
        chat_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<(), GatewayError> {
        let (channel, _) = self.channel_of(chat_id).await?;
        let path = format!(
            "/channels/{channel}/messages/{message_id}/reactions/{}/@me",
            percent_encode(emoji)
        );
        self.rest(Method::PUT, &path, None, true).await.map(|_| ())
    }

    /// A streamed reply's edit. A limit is `RateLimited` straight away, so
    /// the editor paces itself.
    async fn edit(&self, chat_id: &str, message_id: &str, text: &str) -> Result<(), GatewayError> {
        let body = json!({ "content": text, "allowed_mentions": { "parse": [] } });
        let hook = self.hooks.lock().unwrap().get(message_id).cloned();
        let path = match hook {
            Some((app, token)) => format!("/webhooks/{app}/{token}/messages/@original"),
            None => {
                let (channel, _) = self.channel_of(chat_id).await?;
                format!("/channels/{channel}/messages/{message_id}")
            }
        };
        self.rest(Method::PATCH, &path, Some(body), false)
            .await
            .map(|_| ())
    }
}

fn client(api: &str) -> reqwest::Client {
    crate::channels::ws::http_client(api, CONNECT_DEADLINE, REQUEST_DEADLINE)
}

/// A route, for an error: the method's target without ids or tokens.
fn route_words(route: &str) -> String {
    let path = route.split_once('/').map_or("", |(_, p)| p);
    let mut parts: Vec<&str> = path.split('/').collect();
    if matches!(parts.first(), Some(&"webhooks") | Some(&"interactions")) && parts.len() > 2 {
        parts[1] = ":id";
        parts[2] = ":token";
    }
    format!("/{}", parts.join("/"))
}

fn is_thread(c: &Value) -> bool {
    matches!(c["type"].as_u64(), Some(10..=12))
}

fn name_of(user: &Value) -> String {
    user["global_name"]
        .as_str()
        .or_else(|| user["username"].as_str())
        .unwrap_or("unknown")
        .to_string()
}

/// What a message carries that ferrule can't read.
fn unread_kind(d: &Value) -> Option<&'static str> {
    if let Some(first) = d["attachments"].as_array().and_then(|a| a.first()) {
        return Some(access::file_kind(first["content_type"].as_str()));
    }
    d["sticker_items"]
        .as_array()
        .is_some_and(|s| !s.is_empty())
        .then_some("sticker")
}

/// The label of the button with `custom_id` on a message.
fn button_label<'a>(message: &'a Value, custom_id: &str) -> Option<&'a str> {
    message["components"]
        .as_array()?
        .iter()
        .flat_map(|row| row["components"].as_array().into_iter().flatten())
        .find(|b| b["custom_id"].as_str() == Some(custom_id))
        .and_then(|b| b["label"].as_str())
}

/// At most `limit` UTF-16 units of `text`.
fn clip_units(text: &str, limit: usize) -> String {
    chunks(text, limit).into_iter().next().unwrap_or_default()
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// What `ferrule doctor` and setup learn about a bot, without changing it.
#[derive(Debug, Clone)]
pub struct Probe {
    pub bot_name: String,
    pub bot_id: String,
    pub app_id: Option<String>,
    /// Whether the MESSAGE_CONTENT intent is on; `None` when it couldn't
    /// be read.
    pub content_intent: Option<bool>,
    pub shards: u64,
    /// Identifies left today, and the day's total.
    pub identify: Option<(u64, u64)>,
}

/// `GET /users/@me`, `/gateway/bot` and `/applications/@me`: reads only.
pub async fn probe(api: &str, token: &str) -> Result<Probe, String> {
    let ch = DiscordChannel::with_api(token, api);
    let me = ch
        .rest(Method::GET, "/users/@me", None, true)
        .await
        .map_err(|e| explain(e.to_string()))?;
    let gw = ch
        .rest(Method::GET, "/gateway/bot", None, true)
        .await
        .map_err(|e| e.to_string())?;
    let app = ch
        .rest(Method::GET, "/applications/@me", None, true)
        .await
        .ok();
    let limit = &gw["session_start_limit"];
    Ok(Probe {
        bot_name: name_of(&me),
        bot_id: me["id"].as_str().unwrap_or("").to_string(),
        app_id: app
            .as_ref()
            .and_then(|a| a["id"].as_str().map(str::to_string)),
        content_intent: app
            .as_ref()
            .and_then(|a| a["flags"].as_u64())
            .map(|f| f & FLAG_CONTENT != 0),
        shards: gw["shards"].as_u64().unwrap_or(1),
        identify: limit["remaining"].as_u64().zip(limit["total"].as_u64()),
    })
}

fn explain(e: String) -> String {
    if e.contains("status 401") {
        "Discord rejected the bot token (401): copy it again from the developer portal → your app → Bot → Reset Token".into()
    } else {
        e
    }
}

/// Registers the slash commands, globally (`ferrule setup`; a daemon never
/// does). Returns how many Discord now has.
pub async fn register_commands(api: &str, token: &str, app_id: &str) -> Result<usize, String> {
    let ch = DiscordChannel::with_api(token, api);
    let body: Vec<Value> = SLASH_COMMANDS
        .iter()
        .map(|(name, description)| {
            json!({
                "name": name,
                "description": description,
                "type": 1,
                "options": [{ "type": 3, "name": "args", "description": "anything after the command", "required": false }],
            })
        })
        .collect();
    let out = ch
        .rest(
            Method::PUT,
            &format!("/applications/{app_id}/commands"),
            Some(Value::Array(body)),
            true,
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(out.as_array().map_or(0, Vec::len))
}

/// The link that adds the bot to a server with the permissions it needs.
pub fn invite_url(app_id: &str) -> String {
    format!(
        "https://discord.com/oauth2/authorize?client_id={app_id}&scope=bot+applications.commands&permissions={INVITE_PERMISSIONS}"
    )
}
