//! Slack adapter (M31): Socket Mode for what arrives (a WebSocket opened
//! with the app-level `xapp-` token, so no public URL is needed), the Web
//! API with the bot's `xoxb-` token for what goes out. `api` is a
//! constructor argument so tests point it at a mock on 127.0.0.1; the
//! socket's address comes from `apps.connections.open`, so the mock names
//! its own.
//!
//! Who gets in (`access`): a DM from an allowed user; an `@mention` in an
//! allowed channel. A DM's chat is its author's id; a mention's chat is
//! `<channel>/<thread_ts>`, and the conversation goes on in that thread.
//!
//! What goes out is converted from Markdown to Slack's mrkdwn (`mrkdwn`),
//! split at 3900 characters. Buttons are Block Kit; a tap arrives as the
//! command it carries. `/ferrule <command>` arrives as `/<command>`.

mod mrkdwn;
mod socket;

pub use mrkdwn::convert as to_mrkdwn;

use crate::channel::{Button, ButtonAction, Channel, ChannelCapabilities};
use crate::channels::access::{self, Access, Dm};
use crate::error::GatewayError;
use crate::message::{InboundMessage, OutboundMessage};
use crate::stream::chunks;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use tokio::time::Instant;

/// Slack's Web API.
pub const API_URL: &str = "https://slack.com/api";

/// One message's text, kept under Slack's 4000 so a split never lands on
/// Slack's own truncation.
pub const MESSAGE_LIMIT: usize = 3900;
/// A `section` block's text holds at most 3000.
const SECTION_LIMIT: usize = 3000;
/// `chat.update` is a Tier 3 method (~50 a minute): a streamed reply edits
/// at most this often.
pub const STREAM_EVERY: Duration = Duration::from_millis(1500);

const REQUEST_DEADLINE: Duration = Duration::from_secs(30);
const CONNECT_DEADLINE: Duration = Duration::from_secs(10);
/// The longest a rate limit is waited out before giving up.
const MAX_WAIT: Duration = Duration::from_secs(60);
/// How many event ids are remembered to drop Slack's redeliveries.
const SEEN_EVENTS: usize = 2000;

/// The scopes the bot token needs (the manifest in docs/slack.md).
pub const BOT_SCOPES: [&str; 7] = [
    "app_mentions:read",
    "chat:write",
    "commands",
    "im:history",
    "im:read",
    "im:write",
    "reactions:write",
];

/// Waits and retries, shortened in tests.
#[derive(Clone)]
struct Timing {
    backoff_min: Duration,
    backoff_max: Duration,
    /// A WebSocket ping this often…
    ping: Duration,
    /// …and a socket with no frame for this long is dead.
    dead: Duration,
    connect: Duration,
    /// Posts to one channel are at least this far apart.
    post_gap: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            ping: Duration::from_secs(30),
            dead: Duration::from_secs(90),
            connect: Duration::from_secs(20),
            post_gap: Duration::from_secs(1),
        }
    }
}

/// Which token a call carries.
#[derive(Clone, Copy)]
enum Token {
    Bot,
    App,
}

pub struct SlackChannel {
    api: String,
    bot_token: String,
    app_token: String,
    client: reqwest::Client,
    access: Access,
    timing: Timing,
    /// The bot's user id (`auth.test`).
    bot: Mutex<Option<String>>,
    /// Web API method → when its rate limit lifts.
    blocked: Mutex<HashMap<String, Instant>>,
    /// Channel → when its last post went (or is booked to go).
    posted: Mutex<HashMap<String, Instant>>,
    /// User → DM channel.
    dm_channel: Mutex<HashMap<String, String>>,
    /// Event ids seen, oldest first.
    seen: Mutex<(VecDeque<String>, HashSet<String>)>,
    last_frame: Mutex<Option<SystemTime>>,
    problem: Mutex<Option<String>>,
}

impl SlackChannel {
    /// Real usage: talks to `https://slack.com/api`.
    pub fn new(bot_token: impl Into<String>, app_token: impl Into<String>) -> Self {
        Self::with_api(bot_token, app_token, API_URL)
    }

    /// Tests: an arbitrary API base (a mock on 127.0.0.1).
    pub fn with_api(
        bot_token: impl Into<String>,
        app_token: impl Into<String>,
        api: impl Into<String>,
    ) -> Self {
        let api = api.into().trim_end_matches('/').to_string();
        Self {
            client: crate::channels::ws::http_client(&api, CONNECT_DEADLINE, REQUEST_DEADLINE),
            api,
            bot_token: bot_token.into(),
            app_token: app_token.into(),
            access: Access::new("slack", "Slack user id", vec![], vec![]),
            timing: Timing::default(),
            bot: Mutex::new(None),
            blocked: Mutex::new(HashMap::new()),
            posted: Mutex::new(HashMap::new()),
            dm_channel: Mutex::new(HashMap::new()),
            seen: Mutex::new((VecDeque::new(), HashSet::new())),
            last_frame: Mutex::new(None),
            problem: Mutex::new(None),
        }
    }

    /// Who may talk to the bot: users in DMs, channels when it's mentioned.
    pub fn with_allowed(mut self, users: Vec<String>, channels: Vec<String>) -> Self {
        self.access = Access::new("slack", "Slack user id", users, channels);
        self
    }

    /// Setup only: the first DM that is exactly `code` pairs its author.
    pub fn with_pairing(mut self, code: impl Into<String>) -> Self {
        let access = std::mem::replace(
            &mut self.access,
            Access::new("slack", "Slack user id", vec![], vec![]),
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
            ping: Duration::from_millis(200),
            dead: Duration::from_secs(2),
            connect: Duration::from_secs(5),
            post_gap: Duration::from_millis(20),
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

    /// Whether `event_id` is new (and remembers it).
    fn first_time(&self, event_id: &str) -> bool {
        let mut seen = self.seen.lock().unwrap();
        if !seen.1.insert(event_id.to_string()) {
            return false;
        }
        seen.0.push_back(event_id.to_string());
        if seen.0.len() > SEEN_EVENTS {
            if let Some(old) = seen.0.pop_front() {
                seen.1.remove(&old);
            }
        }
        true
    }

    /// One Web API call; `Err` says why in words, never with a token.
    /// `patient`: wait out a rate limit (≤ 60 s) and retry up to three
    /// times; otherwise a limit is `RateLimited` at once.
    async fn call(
        &self,
        method: &str,
        body: Value,
        token: Token,
        patient: bool,
    ) -> Result<Value, GatewayError> {
        let mut tries = 0;
        loop {
            tries += 1;
            let until = self.blocked.lock().unwrap().get(method).copied();
            if let Some(until) = until {
                let wait = until.saturating_duration_since(Instant::now());
                if !wait.is_zero() {
                    if !patient || wait > MAX_WAIT {
                        return Err(GatewayError::RateLimited { retry_after: wait });
                    }
                    tokio::time::sleep(wait).await;
                }
            }
            let token = match token {
                Token::Bot => &self.bot_token,
                Token::App => &self.app_token,
            };
            let resp = self
                .client
                .post(format!("{}/{method}", self.api))
                .bearer_auth(token)
                .json(&body)
                .send()
                .await
                .map_err(|e| {
                    GatewayError::Channel(format!("slack {method} failed: {}", e.without_url()))
                })?;
            let status = resp.status();
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<f64>().ok());
            let text = resp.text().await.unwrap_or_default();
            let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let limited = status.as_u16() == 429 || json["error"] == "ratelimited";
            if limited {
                let retry = Duration::from_secs_f64(retry_after.unwrap_or(1.0).clamp(0.0, 3600.0));
                self.blocked
                    .lock()
                    .unwrap()
                    .insert(method.to_string(), Instant::now() + retry);
                tracing::warn!(
                    retry_after_ms = retry.as_millis() as u64,
                    "slack: rate limited on {method}"
                );
                if patient && tries < 3 && retry <= MAX_WAIT {
                    continue;
                }
                return Err(GatewayError::RateLimited { retry_after: retry });
            }
            if !status.is_success() {
                return Err(GatewayError::Channel(format!(
                    "slack {method} failed (status {status}): {}",
                    crate::health::clip(&text, 300)
                )));
            }
            if json["ok"].as_bool() != Some(true) {
                let error = json["error"].as_str().unwrap_or("no answer");
                return Err(GatewayError::Channel(format!(
                    "slack {method} failed: {error}"
                )));
            }
            return Ok(json);
        }
    }

    /// Waits for `channel`'s turn to post: one post a second per channel.
    async fn pace(&self, channel: &str) {
        let slot = {
            let mut posted = self.posted.lock().unwrap();
            let now = Instant::now();
            let slot = posted
                .get(channel)
                .map_or(now, |last| (*last + self.timing.post_gap).max(now));
            posted.insert(channel.to_string(), slot);
            slot
        };
        tokio::time::sleep_until(slot).await;
    }

    /// Where a chat's messages go: the channel, and the thread in it. A
    /// DM is keyed by its user, so the DM channel is looked up (or opened).
    async fn target(&self, chat: &str) -> Result<(String, Option<String>), GatewayError> {
        if let Some((channel, thread)) = chat.split_once('/') {
            return Ok((channel.to_string(), Some(thread.to_string())));
        }
        if !is_user(chat) {
            return Ok((chat.to_string(), None));
        }
        if let Some(ch) = self.dm_channel.lock().unwrap().get(chat).cloned() {
            return Ok((ch, None));
        }
        let open = self
            .call(
                "conversations.open",
                json!({ "users": chat }),
                Token::Bot,
                true,
            )
            .await?;
        let ch = open["channel"]["id"]
            .as_str()
            .ok_or_else(|| GatewayError::Channel("slack: opening a DM gave no channel".into()))?
            .to_string();
        self.remember_dm(chat, &ch);
        Ok((ch, None))
    }

    fn remember_dm(&self, user: &str, channel: &str) {
        self.dm_channel
            .lock()
            .unwrap()
            .insert(user.to_string(), channel.to_string());
    }

    /// `chat.postMessage`; the new message's `ts`.
    async fn post_in(
        &self,
        channel: &str,
        thread: Option<&str>,
        text: &str,
        blocks: Option<Value>,
    ) -> Result<Option<String>, GatewayError> {
        let mut body = json!({
            "channel": channel,
            "text": text,
            "unfurl_links": false,
            "unfurl_media": false,
        });
        if let Some(t) = thread {
            body["thread_ts"] = json!(t);
        }
        if let Some(b) = blocks {
            body["blocks"] = b;
        }
        self.pace(channel).await;
        let m = self
            .call("chat.postMessage", body, Token::Bot, true)
            .await?;
        Ok(m["ts"].as_str().map(str::to_string))
    }

    /// Sends `msg` as mrkdwn, split; the last message's `ts`. With
    /// `buttons`, the last piece is a section with an actions block.
    async fn deliver(
        &self,
        msg: &OutboundMessage,
        buttons: Option<Value>,
    ) -> Result<Option<String>, GatewayError> {
        let (channel, thread) = self.target(&msg.chat_id).await?;
        let limit = if buttons.is_some() {
            SECTION_LIMIT
        } else {
            MESSAGE_LIMIT
        };
        let mut pieces = chunks(&mrkdwn::convert(&msg.text), limit);
        pieces.retain(|p| !p.trim().is_empty());
        if pieces.is_empty() {
            pieces.push("…".into());
        }
        let last = pieces.len() - 1;
        let mut ts = None;
        for (i, piece) in pieces.iter().enumerate() {
            let blocks = match (&buttons, i == last) {
                (Some(actions), true) => Some(json!([
                    { "type": "section", "text": { "type": "mrkdwn", "text": piece } },
                    actions,
                ])),
                _ => None,
            };
            ts = self
                .post_in(&channel, thread.as_deref(), piece, blocks)
                .await?;
        }
        Ok(ts)
    }

    /// Something to say outside a turn (a stranger's id, the pairing
    /// answer, "I can only read text").
    async fn say(&self, channel: &str, thread: Option<&str>, text: String) {
        if let Err(e) = self.post_in(channel, thread, &text, None).await {
            tracing::warn!(error = %e, "slack: couldn't answer a message");
        }
    }

    /// An `events_api` payload, if it's for the agent.
    pub(super) async fn on_event(&self, p: &Value) -> Option<InboundMessage> {
        if let Some(id) = p["event_id"].as_str() {
            if !self.first_time(id) {
                tracing::debug!("slack: dropped a redelivered event");
                return None;
            }
        }
        let e = &p["event"];
        let kind = e["type"].as_str()?;
        let bot = self.bot_id();
        let user = e["user"].as_str()?.to_string();
        if e["bot_id"].is_string() || Some(&user) == bot.as_ref() {
            return None;
        }
        if e["subtype"].as_str().is_some_and(|s| s != "file_share") {
            return None;
        }
        let sender = name_of(e, &user);
        let channel = e["channel"].as_str()?.to_string();
        let ts = e["ts"].as_str()?.to_string();
        let mut text = e["text"].as_str().unwrap_or("").to_string();
        let unread = e["files"]
            .as_array()
            .and_then(|f| f.first())
            .map(|f| access::file_kind(f["mimetype"].as_str()));
        let im = e["channel_type"] == "im" || channel.starts_with('D');
        let (chat_id, thread) = match kind {
            "message" if im => {
                self.remember_dm(&user, &channel);
                match self.access.dm(&user, &sender, &text) {
                    Dm::Admit => {}
                    Dm::Tell(t) | Dm::Paired(t) => {
                        self.say(&channel, None, t).await;
                        return None;
                    }
                    Dm::Drop => return None,
                }
                (user.clone(), None)
            }
            // A DM's mention also arrives as a `message`.
            "app_mention" if !im => {
                if !self.access.channel_allowed(&channel) {
                    self.access.ignore(&channel, &sender, "channel message");
                    return None;
                }
                if let Some(b) = &bot {
                    text = access::strip_mention(&text, b);
                }
                let thread = e["thread_ts"].as_str().unwrap_or(&ts).to_string();
                (format!("{channel}/{thread}"), Some(thread))
            }
            _ => return None,
        };
        if text.trim().is_empty() {
            if let Some(kind) = unread {
                tracing::info!(chat = %chat_id, "slack: got a {kind} with no text; ferrule reads only text");
                self.say(&channel, thread.as_deref(), access::cannot_read_text(kind))
                    .await;
            }
            return None;
        }
        if let Some(kind) = unread {
            text = access::unread_note(&text, kind);
        }
        Some(InboundMessage {
            channel: "slack".into(),
            chat_id,
            sender,
            sender_id: Some(user),
            message_id: ts.clone(),
            text: unescape(&text),
            attachments: vec![],
            reply_to: None,
            ts: ts.parse::<f64>().map_or(0, |t| t as i64),
        })
    }

    /// An `interactive` payload: a button tap (`block_actions`) arrives as
    /// the command it carries, after the buttons give way to the choice.
    pub(super) async fn on_action(&self, p: &Value) -> Option<InboundMessage> {
        if p["type"] != "block_actions" {
            return None;
        }
        let action = p["actions"].as_array()?.first()?;
        let command = action["value"].as_str()?;
        if !action["action_id"]
            .as_str()
            .is_some_and(|a| a.starts_with("cmd_"))
        {
            return None;
        }
        let user = p["user"]["id"].as_str()?.to_string();
        let sender = p["user"]["name"]
            .as_str()
            .or_else(|| p["user"]["username"].as_str())
            .unwrap_or(&user)
            .to_string();
        let channel = p["channel"]["id"]
            .as_str()
            .or_else(|| p["container"]["channel_id"].as_str())?
            .to_string();
        let (chat_id, admitted) = if channel.starts_with('D') {
            self.remember_dm(&user, &channel);
            (user.clone(), self.access.user_allowed(&user))
        } else {
            let thread = p["message"]["thread_ts"]
                .as_str()
                .or_else(|| p["container"]["thread_ts"].as_str());
            let chat = match thread {
                Some(t) => format!("{channel}/{t}"),
                None => channel.clone(),
            };
            (chat, self.access.channel_allowed(&channel))
        };
        if !admitted {
            self.access.ignore(&chat_id, &sender, "button tap");
            return None;
        }
        if let Some(ts) = p["container"]["message_ts"]
            .as_str()
            .or_else(|| p["message"]["ts"].as_str())
        {
            let label = action["text"]["text"].as_str().unwrap_or("done");
            let text = format!(
                "{}\n→ {}",
                p["message"]["text"].as_str().unwrap_or(""),
                mrkdwn::convert(label)
            );
            let body = json!({
                "channel": channel, "ts": ts, "text": text,
                "blocks": [{ "type": "section", "text": { "type": "mrkdwn", "text": clip(&text, SECTION_LIMIT) } }],
            });
            if let Err(e) = self.call("chat.update", body, Token::Bot, true).await {
                tracing::warn!(error = %e, "slack: couldn't mark a tapped button");
            }
        }
        Some(InboundMessage {
            channel: "slack".into(),
            chat_id,
            sender,
            sender_id: Some(user),
            // No message of the user's to react to or reply to.
            message_id: String::new(),
            text: command.to_string(),
            attachments: vec![],
            reply_to: None,
            ts: chrono::Utc::now().timestamp(),
        })
    }

    /// A `slash_commands` payload: `/ferrule status now` arrives as
    /// `/status now`, answered in the channel when it's allowed, else in
    /// the user's DM when they are, else not at all.
    pub(super) async fn on_slash(&self, p: &Value) -> Option<InboundMessage> {
        let user = p["user_id"].as_str()?.to_string();
        let sender = p["user_name"].as_str().unwrap_or(&user).to_string();
        let channel = p["channel_id"].as_str().unwrap_or("").to_string();
        let words = p["text"].as_str().unwrap_or("").trim();
        let text = if words.is_empty() {
            "/status".to_string()
        } else {
            format!("/{}", words.trim_start_matches('/'))
        };
        let chat_id = if !channel.starts_with('D') && self.access.channel_allowed(&channel) {
            channel
        } else if self.access.user_allowed(&user) {
            if channel.starts_with('D') {
                self.remember_dm(&user, &channel);
            }
            user.clone()
        } else {
            self.access.ignore(&user, &sender, "DM");
            return None;
        };
        Some(InboundMessage {
            channel: "slack".into(),
            chat_id,
            sender,
            sender_id: Some(user),
            message_id: String::new(),
            text,
            attachments: vec![],
            reply_to: None,
            ts: chrono::Utc::now().timestamp(),
        })
    }

    /// The actions block for `buttons`.
    fn actions(buttons: &[Button]) -> Result<Value, GatewayError> {
        if buttons.len() > 25 {
            return Err(GatewayError::Unsupported("more than 25 buttons"));
        }
        let elements: Vec<Value> = buttons
            .iter()
            .enumerate()
            .map(|(n, b)| {
                let text = json!({ "type": "plain_text", "text": clip(&b.text, 75), "emoji": true });
                match &b.action {
                    ButtonAction::Url(url) => Ok(
                        json!({ "type": "button", "text": text, "url": url, "action_id": format!("url_{n}") }),
                    ),
                    ButtonAction::Command(cmd) if cmd.len() <= 2000 => Ok(
                        json!({ "type": "button", "text": text, "value": cmd, "action_id": format!("cmd_{n}") }),
                    ),
                    ButtonAction::Command(_) => Err(GatewayError::Unsupported(
                        "a button command longer than 2000 bytes",
                    )),
                }
            })
            .collect::<Result<_, _>>()?;
        Ok(json!({ "type": "actions", "elements": elements }))
    }
}

#[async_trait::async_trait]
impl Channel for SlackChannel {
    fn name(&self) -> &str {
        "slack"
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

    /// The last frame on the socket: ferrule pings every 30 s, so a quiet
    /// but healthy socket stays fresh.
    fn last_ok_poll(&self) -> Option<SystemTime> {
        *self.last_frame.lock().unwrap()
    }

    fn problem(&self) -> Option<String> {
        self.problem.lock().unwrap().clone()
    }

    fn message_limit(&self) -> Option<usize> {
        Some(MESSAGE_LIMIT)
    }

    fn stream_every(&self) -> Option<Duration> {
        Some(STREAM_EVERY)
    }

    async fn run(&self, tx: tokio::sync::mpsc::Sender<InboundMessage>) -> Result<(), GatewayError> {
        self.run_socket(tx).await
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
        let actions = Self::actions(buttons)?;
        self.deliver(&msg, Some(actions)).await.map(|_| ())
    }

    /// The 👀 receipt: Slack takes emoji by name.
    async fn react(
        &self,
        chat_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<(), GatewayError> {
        let (channel, _) = self.target(chat_id).await?;
        let body =
            json!({ "channel": channel, "timestamp": message_id, "name": emoji_name(emoji) });
        match self.call("reactions.add", body, Token::Bot, true).await {
            Err(e) if e.to_string().ends_with("already_reacted") => Ok(()),
            r => r.map(|_| ()),
        }
    }

    /// A streamed reply's edit. A limit is `RateLimited` straight away, so
    /// the editor paces itself.
    async fn edit(&self, chat_id: &str, message_id: &str, text: &str) -> Result<(), GatewayError> {
        let (channel, _) = self.target(chat_id).await?;
        let body = json!({ "channel": channel, "ts": message_id, "text": mrkdwn::convert(text) });
        self.call("chat.update", body, Token::Bot, false)
            .await
            .map(|_| ())
    }
}

/// Slack user ids start with `U` or `W`; channels with `C`, `G` or `D`.
fn is_user(id: &str) -> bool {
    id.starts_with('U') || id.starts_with('W')
}

fn name_of(e: &Value, user: &str) -> String {
    let profile = &e["user_profile"];
    ["display_name", "real_name", "name"]
        .iter()
        .filter_map(|k| profile[k].as_str())
        .find(|n| !n.is_empty())
        .unwrap_or(user)
        .to_string()
}

/// Slack escapes `&`, `<` and `>` in what users type; the agent reads
/// them as typed.
fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn emoji_name(emoji: &str) -> String {
    match emoji {
        "👀" => "eyes".into(),
        "✅" => "white_check_mark".into(),
        "👍" => "+1".into(),
        "❌" => "x".into(),
        other => other.trim_matches(':').to_string(),
    }
}

/// At most `limit` UTF-16 units of `text`.
fn clip(text: &str, limit: usize) -> String {
    chunks(text, limit).into_iter().next().unwrap_or_default()
}

/// Whether the tokens look like what Slack issues: the bot's `xoxb-` and
/// the app-level `xapp-`. The words say which was pasted where.
pub fn check_tokens(bot_token: &str, app_token: &str) -> Result<(), String> {
    if bot_token.starts_with("xapp-") && app_token.starts_with("xoxb-") {
        return Err("the Slack bot and app tokens are swapped: the bot token starts with xoxb-, the app-level token with xapp-".into());
    }
    if !bot_token.starts_with("xoxb-") {
        return Err("the Slack bot token should start with xoxb- (api.slack.com → your app → OAuth & Permissions → Bot User OAuth Token)".into());
    }
    if !app_token.starts_with("xapp-") {
        return Err("the Slack app token should start with xapp- (api.slack.com → your app → Basic Information → App-Level Tokens, with connections:write)".into());
    }
    Ok(())
}

/// What `ferrule doctor` and setup learn about an app, without changing it.
#[derive(Debug, Clone)]
pub struct Probe {
    pub bot_name: String,
    /// The bot's user id.
    pub bot_id: String,
    pub team: String,
    /// Whether the app token may open a Socket Mode connection.
    pub socket: Result<(), String>,
    /// The bot token's scopes, from `auth.test`'s `x-oauth-scopes` header
    /// (`None` when Slack didn't send it).
    pub scopes: Option<Vec<String>>,
}

impl Probe {
    /// The scopes in [`BOT_SCOPES`] the bot token lacks; empty when they're
    /// all there or Slack didn't say.
    pub fn missing_scopes(&self) -> Vec<&'static str> {
        match &self.scopes {
            Some(have) => BOT_SCOPES
                .iter()
                .copied()
                .filter(|s| !have.iter().any(|h| h == s))
                .collect(),
            None => vec![],
        }
    }
}

/// `auth.test` with the bot token and `apps.connections.open` with the app
/// token (a URL that's never used): nothing is posted or changed.
pub async fn probe(api: &str, bot_token: &str, app_token: &str) -> Result<Probe, String> {
    let ch = SlackChannel::with_api(bot_token, app_token, api);
    let me = ch
        .call("auth.test", json!({}), Token::Bot, true)
        .await
        .map_err(|e| explain(e.to_string(), Token::Bot))?;
    let socket = ch
        .call("apps.connections.open", json!({}), Token::App, true)
        .await
        .map(|_| ())
        .map_err(|e| explain(e.to_string(), Token::App));
    // Only a header carries the scopes, so ask once more, plainly.
    let scopes = ch
        .client
        .post(format!("{}/auth.test", ch.api))
        .bearer_auth(bot_token)
        .send()
        .await
        .ok()
        .and_then(|r| {
            let v = r.headers().get("x-oauth-scopes")?.to_str().ok()?;
            Some(
                v.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
            )
        });
    Ok(Probe {
        bot_name: me["user"].as_str().unwrap_or("").to_string(),
        bot_id: me["user_id"].as_str().unwrap_or("").to_string(),
        team: me["team"].as_str().unwrap_or("").to_string(),
        socket,
        scopes,
    })
}

fn explain(e: String, token: Token) -> String {
    let bad_token = [
        "invalid_auth",
        "not_authed",
        "account_inactive",
        "token_revoked",
        "not_allowed_token_type",
    ]
    .iter()
    .any(|k| e.ends_with(k));
    match (bad_token, token) {
        (true, Token::Bot) => format!("Slack rejected the bot token ({}): copy the Bot User OAuth Token (xoxb-…) again from api.slack.com → your app → OAuth & Permissions", last_word(&e)),
        (true, Token::App) => format!("Slack rejected the app token ({}): make an App-Level Token (xapp-…) with the connections:write scope at api.slack.com → your app → Basic Information", last_word(&e)),
        _ if e.ends_with("missing_scope") => format!("{e}: the app lacks a scope — add {} under OAuth & Permissions and reinstall it", BOT_SCOPES.join(", ")),
        _ => e,
    }
}

fn last_word(e: &str) -> &str {
    e.rsplit(' ').next().unwrap_or(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_checked_by_their_shape() {
        assert!(check_tokens("xoxb-1", "xapp-1").is_ok());
        assert!(check_tokens("xapp-1", "xoxb-1")
            .unwrap_err()
            .contains("swapped"));
        assert!(check_tokens("xoxp-1", "xapp-1")
            .unwrap_err()
            .contains("xoxb-"));
        assert!(check_tokens("xoxb-1", "xoxb-2")
            .unwrap_err()
            .contains("xapp-"));
    }

    #[test]
    fn users_and_channels_are_told_apart_and_emoji_named() {
        assert!(is_user("U012AB") && is_user("W1"));
        assert!(!is_user("C1") && !is_user("D1") && !is_user("G1"));
        assert_eq!(emoji_name("👀"), "eyes");
        assert_eq!(emoji_name(":tada:"), "tada");
        assert_eq!(unescape("a &lt;b&gt; &amp;amp;"), "a <b> &amp;");
    }
}
