//! Mattermost adapter (M39 §7): REST under `/api/v4` for everything that
//! goes out, and the WebSocket (`socket`) for what comes in.
//!
//! Login is a bot account's token (or a personal access token), sent as a
//! bearer header and in the socket's `authentication_challenge`; never
//! logged.
//!
//! Who gets in: a DM from an allow-listed user (ids, or usernames resolved
//! once at start); a channel on `allowed_channels` when the bot is
//! mentioned, or in a thread it has answered in. A DM's chat id is the
//! *user* id, so an owner notice or a task's result reaches the person; the
//! DM channel is looked up (`POST /channels/direct`) when something is sent.
//! A channel's chat is `<channel id>/<thread root>`, so an answer stays in
//! the thread the question started.
//!
//! Approvals: Mattermost's interactive buttons call back a URL a local
//! gateway doesn't have, so the answers are listed as text and the bot
//! reacts with one emoji per answer (`+1`, `-1`, `one`…); an allowed user's
//! reaction sends that answer.

mod socket;

use crate::channel::{buttons_as_text, Button, ButtonAction, Channel, ChannelCapabilities};
use crate::channels::access::Access;
use crate::channels::files::{self, Inbox};
use crate::error::GatewayError;
use crate::message::{Attachment, InboundMessage, OutboundMessage};
use crate::stream::chunks;
use reqwest::Method;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

/// A post's text: Mattermost's own cap (`MaxPostSize`, 16 383 runes).
pub const MESSAGE_LIMIT: usize = 16_383;
/// Files one post carries.
const FILES_PER_POST: usize = 5;
const REQUEST_DEADLINE: Duration = Duration::from_secs(30);
const CONNECT_DEADLINE: Duration = Duration::from_secs(10);
/// Our own posts (and the threads we answered in) remembered, so a
/// follow-up there counts as addressed to the bot.
const SENT: usize = 500;
/// Approval posts remembered with their reactions.
const APPROVALS: usize = 100;

/// Waits and retries, shortened in tests.
#[derive(Clone)]
struct Timing {
    /// A `ping` action this often.
    ping: Duration,
    /// Nothing heard for this long: the socket is dead.
    dead: Duration,
    connect: Duration,
    backoff_min: Duration,
    backoff_max: Duration,
    /// The longest a 429 is waited out before a call gives up.
    rate_cap: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            ping: Duration::from_secs(30),
            dead: Duration::from_secs(90),
            connect: CONNECT_DEADLINE,
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            rate_cap: Duration::from_secs(30),
        }
    }
}

/// Everything the adapter needs.
#[derive(Clone)]
pub struct MattermostConfig {
    /// `https://chat.example.com`.
    pub server_url: String,
    pub token: String,
    /// Where files people send are saved; `None`: not saved.
    pub inbox: Option<Inbox>,
}

impl std::fmt::Debug for MattermostConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MattermostConfig")
            .field("server_url", &self.server_url)
            .field("token", &"…")
            .field("inbox", &self.inbox.is_some())
            .finish()
    }
}

/// A REST error.
#[derive(Debug, Clone)]
struct MmError {
    status: u16,
    message: String,
    /// `X-Ratelimit-Reset` (or `Retry-After`) on a 429.
    reset: Option<Duration>,
}

impl MmError {
    fn transport(what: &str, e: reqwest::Error) -> Self {
        Self {
            status: 0,
            message: format!("mattermost {what} failed: {}", e.without_url()),
            reset: None,
        }
    }
}

/// The bot's own account.
#[derive(Clone, Debug)]
struct Me {
    id: String,
    username: String,
}

/// An approval: its post, the chat it went to, and (emoji, command).
type Approval = (String, String, Vec<(String, String)>);

pub struct MattermostChannel {
    cfg: MattermostConfig,
    base: String,
    client: reqwest::Client,
    access: Access,
    /// `allowed_users` as given: usernames among them are resolved once.
    wanted: Vec<String>,
    resolved: AtomicBool,
    timing: Timing,
    me: tokio::sync::Mutex<Option<Me>>,
    /// User id → the DM channel with them.
    dms: Mutex<HashMap<String, String>>,
    /// Our posts, and thread roots we posted under.
    sent: Mutex<VecDeque<String>>,
    approvals: Mutex<VecDeque<Approval>>,
    last_poll: Mutex<Option<SystemTime>>,
    /// The token was refused.
    auth_problem: Mutex<Option<String>>,
    /// The socket is failing.
    socket_problem: Mutex<Option<String>>,
}

impl MattermostChannel {
    pub fn new(cfg: MattermostConfig) -> Self {
        let base = cfg.server_url.trim().trim_end_matches('/').to_string();
        Self {
            client: crate::channels::ws::http_client(&base, CONNECT_DEADLINE, REQUEST_DEADLINE),
            base,
            access: Self::access(vec![], vec![]),
            wanted: vec![],
            resolved: AtomicBool::new(true),
            timing: Timing::default(),
            me: tokio::sync::Mutex::new(None),
            dms: Mutex::new(HashMap::new()),
            sent: Mutex::new(VecDeque::new()),
            approvals: Mutex::new(VecDeque::new()),
            last_poll: Mutex::new(None),
            auth_problem: Mutex::new(None),
            socket_problem: Mutex::new(None),
            cfg,
        }
    }

    fn access(users: Vec<String>, channels: Vec<String>) -> Access {
        Access::new("mattermost", "Mattermost user id", users, channels).with_keys(
            "[gateway.mattermost] allowed_users",
            "[gateway.mattermost] allowed_channels",
        )
    }

    /// Who may talk to the bot (user ids, or usernames resolved at start),
    /// and the channels it answers in when mentioned (channel ids).
    pub fn with_allowed(mut self, users: Vec<String>, channels: Vec<String>) -> Self {
        let ids = users.iter().filter(|u| is_id(u)).cloned().collect();
        self.resolved = AtomicBool::new(users.iter().all(|u| is_id(u)));
        self.wanted = users;
        self.access = Self::access(ids, channels);
        self
    }

    /// Setup only: the first DM that is exactly `code` pairs its author.
    pub fn with_pairing(mut self, code: impl Into<String>) -> Self {
        let access = std::mem::replace(&mut self.access, Self::access(vec![], vec![]));
        self.access = access.with_pairing(code);
        self
    }

    /// Who paired during setup, `(user id, username)`.
    pub fn paired(&self) -> Option<(String, String)> {
        self.access.paired()
    }

    /// Tests: waits in milliseconds, not seconds.
    #[doc(hidden)]
    pub fn with_fast_retries(mut self) -> Self {
        self.timing = Timing {
            ping: Duration::from_millis(200),
            dead: Duration::from_millis(1500),
            connect: Duration::from_secs(2),
            backoff_min: Duration::from_millis(20),
            backoff_max: Duration::from_millis(100),
            rate_cap: Duration::from_millis(300),
        };
        self
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/v4{path}", self.base)
    }

    fn frame(&self) {
        *self.last_poll.lock().unwrap() = Some(SystemTime::now());
    }

    fn set_socket_problem(&self, p: Option<String>) {
        *self.socket_problem.lock().unwrap() = p;
    }

    /// Sends `req` with the token; the response when it succeeded, else
    /// the server's error. A 429 is waited out (`X-Ratelimit-Reset`) up to
    /// three times. Never says the token.
    async fn http(
        &self,
        build: impl Fn() -> reqwest::RequestBuilder,
        what: &str,
    ) -> Result<reqwest::Response, MmError> {
        let mut tries = 0;
        loop {
            tries += 1;
            let resp = build()
                .bearer_auth(&self.cfg.token)
                .send()
                .await
                .map_err(|e| MmError::transport(what, e))?;
            let status = resp.status();
            if status.is_success() {
                return Ok(resp);
            }
            let reset = ["x-ratelimit-reset", "retry-after"]
                .iter()
                .find_map(|h| {
                    resp.headers()
                        .get(*h)?
                        .to_str()
                        .ok()?
                        .trim()
                        .parse::<u64>()
                        .ok()
                })
                .map(Duration::from_secs);
            let text = resp.text().await.unwrap_or_default();
            if status.as_u16() == 429 && tries <= 3 {
                let wait = reset
                    .unwrap_or(self.timing.backoff_min)
                    .max(Duration::from_millis(50))
                    .min(self.timing.rate_cap);
                tracing::warn!("mattermost: rate limited, waiting {wait:?}");
                tokio::time::sleep(wait).await;
                continue;
            }
            let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let detail = v["message"]
                .as_str()
                .map(|m| crate::health::clip(m, 300))
                .unwrap_or_else(|| format!("status {status}"));
            let e = MmError {
                status: status.as_u16(),
                message: format!("mattermost {what} failed: {detail}"),
                reset,
            };
            if e.status == 401 {
                *self.auth_problem.lock().unwrap() = Some(refused(&e));
            }
            return Err(e);
        }
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        what: &str,
    ) -> Result<Value, MmError> {
        let url = self.url(path);
        let resp = self
            .http(
                || {
                    let req = self.client.request(method.clone(), &url);
                    match body {
                        Some(b) => req.json(b),
                        None => req,
                    }
                },
                what,
            )
            .await?;
        let text = resp.text().await.map_err(|e| MmError::transport(what, e))?;
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// The bot's own account, asked once (`GET /users/me`).
    async fn me(&self) -> Result<Me, MmError> {
        let mut me = self.me.lock().await;
        if let Some(m) = &*me {
            return Ok(m.clone());
        }
        let v = self
            .call(Method::GET, "/users/me", None, "users/me")
            .await?;
        let m = Me {
            id: v["id"].as_str().unwrap_or("").to_string(),
            username: v["username"].as_str().unwrap_or("").to_string(),
        };
        if m.id.is_empty() {
            return Err(MmError {
                status: 0,
                message: format!(
                    "{} didn't say who the token belongs to (is it a Mattermost server?)",
                    self.base
                ),
                reset: None,
            });
        }
        *self.auth_problem.lock().unwrap() = None;
        *me = Some(m.clone());
        Ok(m)
    }

    /// `allowed_users` given as usernames, turned into ids (once).
    async fn resolve_users(&self) {
        if self.resolved.load(Ordering::Relaxed) {
            return;
        }
        let names: Vec<String> = self
            .wanted
            .iter()
            .filter(|u| !is_id(u))
            .map(|u| u.trim().trim_start_matches('@').to_lowercase())
            .collect();
        match self
            .call(
                Method::POST,
                "/users/usernames",
                Some(&json!(names)),
                "users/usernames",
            )
            .await
        {
            Ok(v) => {
                let found: Vec<(String, String)> = v
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|u| {
                        Some((
                            u["username"].as_str()?.to_string(),
                            u["id"].as_str()?.to_string(),
                        ))
                    })
                    .collect();
                for name in &names {
                    match found.iter().find(|(u, _)| u == name) {
                        Some((_, id)) => self.access.allow(id),
                        None => tracing::warn!(
                            "mattermost: no user @{name} on {} ([gateway.mattermost] allowed_users)",
                            self.base
                        ),
                    }
                }
                self.resolved.store(true, Ordering::Relaxed);
            }
            Err(e) => tracing::warn!(
                "mattermost: couldn't look up allowed usernames: {}",
                e.message
            ),
        }
    }

    fn remember_sent(&self, id: &str) {
        let mut sent = self.sent.lock().unwrap();
        if sent.iter().any(|s| s == id) {
            return;
        }
        sent.push_back(id.to_string());
        if sent.len() > SENT {
            sent.pop_front();
        }
    }

    fn is_ours(&self, id: &str) -> bool {
        self.sent.lock().unwrap().iter().any(|s| s == id)
    }

    fn remember_dm(&self, user: &str, channel: &str) {
        self.dms
            .lock()
            .unwrap()
            .insert(user.to_string(), channel.to_string());
    }

    /// Where a chat's posts go: the channel, and the thread in it. A DM is
    /// keyed by its user (an id, or a username), so the DM channel is
    /// looked up (or opened).
    async fn target(&self, chat: &str) -> Result<(String, Option<String>), GatewayError> {
        if let Some((channel, root)) = chat.split_once('/') {
            return Ok((channel.to_string(), Some(root.to_string())));
        }
        if let Some(ch) = self.dms.lock().unwrap().get(chat).cloned() {
            return Ok((ch, None));
        }
        let user = if is_id(chat) {
            if !self.access.user_allowed(chat) {
                return Ok((chat.to_string(), None));
            }
            chat.to_string()
        } else {
            let name = chat.trim_start_matches('@').to_lowercase();
            let v = self
                .call(
                    Method::GET,
                    &format!("/users/username/{}", enc(&name)),
                    None,
                    "users/username",
                )
                .await
                .map_err(|e| {
                    GatewayError::Channel(if e.status == 404 {
                        format!(
                            "mattermost: {chat:?} is neither a channel id nor a username on {}",
                            self.base
                        )
                    } else {
                        e.message
                    })
                })?;
            v["id"].as_str().unwrap_or("").to_string()
        };
        let me = self
            .me()
            .await
            .map_err(|e| GatewayError::Channel(e.message))?;
        let v = self
            .call(
                Method::POST,
                "/channels/direct",
                Some(&json!([me.id, user])),
                "channels/direct",
            )
            .await
            .map_err(|e| GatewayError::Channel(e.message))?;
        let ch = v["id"]
            .as_str()
            .filter(|c| !c.is_empty())
            .ok_or_else(|| {
                GatewayError::Channel("mattermost: opening a DM gave no channel".into())
            })?
            .to_string();
        self.remember_dm(chat, &ch);
        if chat != user {
            self.remember_dm(&user, &ch);
        }
        Ok((ch, None))
    }

    /// The error a send returns.
    fn explain(&self, channel: &str, e: MmError) -> GatewayError {
        match e.status {
            429 => GatewayError::RateLimited {
                retry_after: e.reset.unwrap_or(self.timing.backoff_max),
            },
            403 => GatewayError::Channel(format!(
                "{} (is the bot a member of {channel}, and allowed to post there?)",
                e.message
            )),
            _ => GatewayError::Channel(e.message),
        }
    }

    /// `POST /posts`; the new post's id.
    async fn post_in(
        &self,
        channel: &str,
        root: Option<&str>,
        text: &str,
        file_ids: &[String],
    ) -> Result<String, GatewayError> {
        let mut body = json!({ "channel_id": channel, "message": text });
        if let Some(r) = root {
            body["root_id"] = json!(r);
        }
        if !file_ids.is_empty() {
            body["file_ids"] = json!(file_ids);
        }
        let v = self
            .call(Method::POST, "/posts", Some(&body), "posts")
            .await
            .map_err(|e| self.explain(channel, e))?;
        let id = v["id"].as_str().unwrap_or("").to_string();
        if !id.is_empty() {
            self.remember_sent(&id);
        }
        if let Some(r) = root {
            self.remember_sent(r);
        }
        Ok(id)
    }

    /// Something to say outside a turn (a stranger's id, the pairing
    /// answer, a refused file).
    async fn tell(&self, channel: &str, root: Option<&str>, text: String) {
        if let Err(e) = self.post_in(channel, root, &text, &[]).await {
            tracing::warn!("mattermost: couldn't answer in {channel}: {e}");
        }
    }

    /// Text and files: all of `msg`; the last post's id. Files ride on
    /// the first posts, five at a time.
    async fn deliver(&self, msg: &OutboundMessage) -> Result<Option<String>, GatewayError> {
        let (channel, root) = self.target(&msg.chat_id).await?;
        let mut ids = vec![];
        for att in &msg.attachments {
            ids.push(self.upload(&channel, att).await?);
        }
        let groups: Vec<&[String]> = ids.chunks(FILES_PER_POST).collect();
        let mut pieces = chunks(&msg.text, MESSAGE_LIMIT);
        pieces.retain(|p| !p.trim().is_empty());
        let mut last = None;
        for i in 0..pieces.len().max(groups.len()) {
            let text = pieces.get(i).map_or("", String::as_str);
            let files = groups.get(i).copied().unwrap_or(&[]);
            last = Some(self.post_in(&channel, root.as_deref(), text, files).await?);
        }
        Ok(last)
    }

    /// `POST /files`; the file's id, for a post's `file_ids`.
    async fn upload(&self, channel: &str, att: &Attachment) -> Result<String, GatewayError> {
        let (name, mime, bytes) = files::read_outgoing(att)?;
        let mime = if att.kind.contains('/') {
            att.kind.clone()
        } else {
            mime.to_string()
        };
        let (boundary, body) =
            files::multipart(&[("channel_id", channel)], ("files", &name, &mime, &bytes));
        let url = self.url("/files");
        let resp = self
            .http(
                || {
                    self.client
                        .post(&url)
                        .header(
                            "content-type",
                            format!("multipart/form-data; boundary={boundary}"),
                        )
                        .body(body.clone())
                },
                "files",
            )
            .await
            .map_err(|e| match e.status {
                413 => GatewayError::Channel(format!(
                    "mattermost: {name} is over the server's file size limit (System Console → File Storage → Maximum File Size)"
                )),
                501 => GatewayError::Channel(
                    "mattermost: file uploads are turned off on this server (System Console → File Sharing)".into(),
                ),
                _ => self.explain(channel, e),
            })?;
        let v: Value = resp
            .json()
            .await
            .map_err(|e| GatewayError::Channel(MmError::transport("files", e).message))?;
        v["file_infos"][0]["id"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| GatewayError::Channel("mattermost: an upload gave no file id".into()))
    }

    /// Downloads a file someone sent into the inbox.
    async fn fetch_file(
        &self,
        id: &str,
        name: &str,
        size: Option<u64>,
        mime: Option<&str>,
    ) -> Result<files::Saved, files::Refused> {
        let refuse = |why: String| files::Refused {
            name: name.to_string(),
            why,
        };
        let Some(inbox) = &self.cfg.inbox else {
            return Err(refuse("this gateway doesn't save files".into()));
        };
        if let Some(n) = size.filter(|n| *n > inbox.max_bytes()) {
            return Err(inbox.too_big(name, n));
        }
        let url = self.url(&format!("/files/{}", enc(id)));
        let resp = self
            .http(|| self.client.get(&url), "file download")
            .await
            .map_err(|e| refuse(e.message))?;
        let bytes = files::read_capped(resp, inbox.max_bytes())
            .await
            .map_err(refuse)?;
        inbox
            .save("mattermost", id, name, mime, &bytes)
            .map_err(|e| refuse(format!("it couldn't be saved: {e}")))
    }

    /// An approval: the answers listed as text, and one reaction per
    /// answer on the post, so a tap sends it.
    async fn deliver_buttons(
        &self,
        msg: &OutboundMessage,
        buttons: &[Button],
    ) -> Result<(), GatewayError> {
        let mut keyed: Vec<(String, String, String)> = vec![];
        let mut digit = 0;
        for b in buttons {
            if let ButtonAction::Command(cmd) = &b.action {
                let key = if cmd.starts_with("yes ") && !keyed.iter().any(|(k, _, _)| k == "+1") {
                    "+1".to_string()
                } else if cmd.starts_with("no ") && !keyed.iter().any(|(k, _, _)| k == "-1") {
                    "-1".to_string()
                } else {
                    digit += 1;
                    match DIGITS.get(digit - 1) {
                        Some(d) => d.to_string(),
                        None => continue,
                    }
                };
                keyed.push((key, b.text.clone(), cmd.clone()));
            }
        }
        let mut text = msg.text.clone();
        for (key, label, cmd) in &keyed {
            text.push_str(&format!(
                "\n• :{key}: {label}: react :{key}:, or send `{cmd}`"
            ));
        }
        let links: Vec<Button> = buttons
            .iter()
            .filter(|b| matches!(b.action, ButtonAction::Url(_)))
            .cloned()
            .collect();
        let out = OutboundMessage {
            text: buttons_as_text(&text, &links),
            ..msg.clone()
        };
        let Some(id) = self.deliver(&out).await? else {
            return Ok(());
        };
        if keyed.is_empty() || id.is_empty() {
            return Ok(());
        }
        {
            let mut approvals = self.approvals.lock().unwrap();
            approvals.push_back((
                id.clone(),
                msg.chat_id.clone(),
                keyed
                    .iter()
                    .map(|(k, _, c)| (k.clone(), c.clone()))
                    .collect(),
            ));
            if approvals.len() > APPROVALS {
                approvals.pop_front();
            }
        }
        for (key, _, _) in &keyed {
            if let Err(e) = self.add_reaction(&id, key).await {
                tracing::warn!(
                    "mattermost: couldn't add the :{key}: reaction: {}",
                    e.message
                );
            }
        }
        Ok(())
    }

    /// The chat and command an allowed user's reaction on an approval
    /// stands for (used once).
    fn approval_answer(&self, post: &str, emoji: &str) -> Option<(String, String)> {
        let mut approvals = self.approvals.lock().unwrap();
        let n = approvals.iter().position(|(id, _, _)| id == post)?;
        let cmd = approvals[n]
            .2
            .iter()
            .find(|(k, _)| k == emoji)
            .map(|(_, c)| c.clone())?;
        let chat = approvals[n].1.clone();
        approvals.remove(n);
        Some((chat, cmd))
    }

    async fn add_reaction(&self, post: &str, emoji: &str) -> Result<(), MmError> {
        let me = self.me().await?;
        let body = json!({ "user_id": me.id, "post_id": post, "emoji_name": emoji });
        self.call(Method::POST, "/reactions", Some(&body), "reactions")
            .await
            .map(|_| ())
    }
}

/// The reactions an approval offers after 👍/👎.
const DIGITS: [&str; 9] = [
    "one", "two", "three", "four", "five", "six", "seven", "eight", "nine",
];

/// Mattermost's name for an emoji ferrule reacts with; a name passes as
/// is.
fn emoji_name(emoji: &str) -> Option<&str> {
    Some(match emoji.trim_end_matches('\u{fe0f}') {
        "👀" => "eyes",
        "👍" => "+1",
        "👎" => "-1",
        "✅" => "white_check_mark",
        "❌" => "x",
        "⏳" => "hourglass_flowing_sand",
        e if !e.is_empty()
            && e.chars()
                .all(|c| c.is_ascii_alphanumeric() || "_+-".contains(c)) =>
        {
            e
        }
        _ => return None,
    })
}

/// Whether `s` looks like a Mattermost id: 26 lowercase letters and digits.
pub fn is_id(s: &str) -> bool {
    s.len() == 26
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// A path segment, percent-encoded.
fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A 401, in words the owner can act on.
fn refused(e: &MmError) -> String {
    format!(
        "the Mattermost token was refused (revoked, or the bot account disabled?): {}. Make a new token (System Console → Integrations → Bot Accounts) and run `ferrule setup` → Mattermost",
        e.message
    )
}

/// What the probe learned about the bot's account.
#[derive(Debug, Clone)]
pub struct Probe {
    pub user_id: String,
    pub username: String,
    pub is_bot: bool,
    /// The server's host, for the summary.
    pub host: String,
}

impl Probe {
    /// "@ferrule on chat.example.com".
    pub fn summary(&self) -> String {
        let mut s = format!("@{} on {}", self.username, self.host);
        if !self.is_bot {
            s.push_str(" (a person's account, not a bot account)");
        }
        s
    }
}

/// A channel the bot is in, for setup.
#[derive(Debug, Clone)]
pub struct ChannelInfo {
    pub id: String,
    /// "Town Square", or the channel's name when it has no display name.
    pub name: String,
    pub team: String,
    /// `O` public, `P` private.
    pub kind: String,
}

fn server_ok(url: &str) -> Result<(), String> {
    if url.starts_with("https://") || url.starts_with("http://") {
        Ok(())
    } else {
        Err(format!(
            "the server should be a URL like https://chat.example.com, not {url:?}"
        ))
    }
}

/// Checks the token (`GET /users/me`): nothing is sent.
pub async fn probe(cfg: MattermostConfig) -> Result<Probe, String> {
    server_ok(&cfg.server_url)?;
    let ch = MattermostChannel::new(cfg);
    let v = ch
        .call(Method::GET, "/users/me", None, "users/me")
        .await
        .map_err(|e| match e.status {
            401 => refused(&e),
            404 | 405 => format!(
                "{} doesn't answer as a Mattermost server (no /api/v4): use the address you open Mattermost at",
                ch.base
            ),
            _ => e.message,
        })?;
    let (Some(id), Some(username)) = (v["id"].as_str(), v["username"].as_str()) else {
        return Err(format!(
            "{} didn't say who the token belongs to (is it a Mattermost server?)",
            ch.base
        ));
    };
    let host = ch
        .base
        .split_once("://")
        .map_or(ch.base.as_str(), |(_, h)| h)
        .to_string();
    Ok(Probe {
        user_id: id.into(),
        username: username.into(),
        is_bot: v["is_bot"].as_bool().unwrap_or(false),
        host,
    })
}

/// The public and private channels the bot is in, in every team.
pub async fn channels(cfg: MattermostConfig) -> Result<Vec<ChannelInfo>, String> {
    server_ok(&cfg.server_url)?;
    let ch = MattermostChannel::new(cfg);
    let teams = ch
        .call(Method::GET, "/users/me/teams", None, "teams")
        .await
        .map_err(|e| e.message)?;
    let mut out = vec![];
    for t in teams.as_array().into_iter().flatten() {
        let (Some(tid), team) = (t["id"].as_str(), t["display_name"].as_str().unwrap_or("")) else {
            continue;
        };
        let list = ch
            .call(
                Method::GET,
                &format!("/users/me/teams/{}/channels", enc(tid)),
                None,
                "channels",
            )
            .await
            .map_err(|e| e.message)?;
        for c in list.as_array().into_iter().flatten() {
            let kind = c["type"].as_str().unwrap_or("");
            if !matches!(kind, "O" | "P") {
                continue;
            }
            let name = c["display_name"]
                .as_str()
                .filter(|n| !n.is_empty())
                .or(c["name"].as_str())
                .unwrap_or("")
                .to_string();
            out.push(ChannelInfo {
                id: c["id"].as_str().unwrap_or("").to_string(),
                name,
                team: team.to_string(),
                kind: kind.to_string(),
            });
        }
    }
    Ok(out)
}

/// A username's id (`@max` or `max`), for setup.
pub async fn user_id(cfg: MattermostConfig, username: &str) -> Result<String, String> {
    server_ok(&cfg.server_url)?;
    let ch = MattermostChannel::new(cfg);
    let name = username.trim().trim_start_matches('@').to_lowercase();
    let v = ch
        .call(
            Method::GET,
            &format!("/users/username/{}", enc(&name)),
            None,
            "users/username",
        )
        .await
        .map_err(|e| {
            if e.status == 404 {
                format!("there is no user @{name} on {}", ch.base)
            } else {
                e.message
            }
        })?;
    v["id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("{} gave no id for @{name}", ch.base))
}

#[async_trait::async_trait]
impl Channel for MattermostChannel {
    fn name(&self) -> &str {
        "mattermost"
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            reactions: true,
            edits: true,
            attachments: true,
            buttons: true,
        }
    }

    fn polls(&self) -> bool {
        true
    }

    fn last_ok_poll(&self) -> Option<SystemTime> {
        *self.last_poll.lock().unwrap()
    }

    fn problem(&self) -> Option<String> {
        self.auth_problem
            .lock()
            .unwrap()
            .clone()
            .or_else(|| self.socket_problem.lock().unwrap().clone())
    }

    fn message_limit(&self) -> Option<usize> {
        Some(MESSAGE_LIMIT)
    }

    /// Servers limit calls per second (10 by default), and an edit is a
    /// call: a streamed answer is edited every 2 s at most.
    fn stream_every(&self) -> Option<Duration> {
        Some(Duration::from_secs(2))
    }

    async fn run(&self, tx: tokio::sync::mpsc::Sender<InboundMessage>) -> Result<(), GatewayError> {
        self.run_socket(tx).await
    }

    async fn send(&self, msg: OutboundMessage) -> Result<(), GatewayError> {
        self.deliver(&msg).await.map(|_| ())
    }

    async fn post(&self, msg: OutboundMessage) -> Result<Option<String>, GatewayError> {
        self.deliver(&msg).await
    }

    async fn send_buttons(
        &self,
        msg: OutboundMessage,
        buttons: &[Button],
    ) -> Result<(), GatewayError> {
        self.deliver_buttons(&msg, buttons).await
    }

    /// The 👀: the channel marked read, and the reaction.
    async fn react(
        &self,
        chat_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<(), GatewayError> {
        if !is_id(message_id) {
            return Ok(());
        }
        let Some(name) = emoji_name(emoji) else {
            return Ok(());
        };
        let (channel, _) = self.target(chat_id).await?;
        if let Err(e) = self
            .call(
                Method::POST,
                "/channels/members/me/view",
                Some(&json!({ "channel_id": channel })),
                "view",
            )
            .await
        {
            tracing::debug!("mattermost: marking read: {}", e.message);
        }
        self.add_reaction(message_id, name)
            .await
            .map_err(|e| self.explain(&channel, e))
    }

    async fn edit(&self, chat_id: &str, message_id: &str, text: &str) -> Result<(), GatewayError> {
        let text = chunks(text, MESSAGE_LIMIT)
            .into_iter()
            .next()
            .unwrap_or_default();
        self.call(
            Method::PUT,
            &format!("/posts/{}/patch", enc(message_id)),
            Some(&json!({ "message": text })),
            "posts/patch",
        )
        .await
        .map(|_| ())
        .map_err(|e| self.explain(chat_id, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_emoji_names() {
        assert!(is_id("q4pmc8ne1ibf5ddkm8bnq5h1ay"));
        assert!(!is_id("max"));
        assert!(!is_id("Q4PMC8NE1IBF5DDKM8BNQ5H1AY"));
        assert_eq!(emoji_name("👀"), Some("eyes"));
        assert_eq!(emoji_name("👍\u{fe0f}"), Some("+1"));
        assert_eq!(emoji_name("white_check_mark"), Some("white_check_mark"));
        assert_eq!(emoji_name("🦀"), None);
        assert_eq!(enc("a b/c"), "a%20b%2Fc");
    }

    #[test]
    fn a_probe_summary_says_a_person_s_token() {
        let p = Probe {
            user_id: "x".into(),
            username: "ferrule".into(),
            is_bot: true,
            host: "chat.example.com".into(),
        };
        assert_eq!(p.summary(), "@ferrule on chat.example.com");
        let p = Probe { is_bot: false, ..p };
        assert!(p
            .summary()
            .ends_with("(a person's account, not a bot account)"));
    }

    #[test]
    fn config_debug_hides_the_token() {
        let c = MattermostConfig {
            server_url: "https://x".into(),
            token: "secret-token-abc".into(),
            inbox: None,
        };
        assert!(!format!("{c:?}").contains("secret-token"));
    }
}
