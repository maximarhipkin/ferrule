//! Matrix adapter (M39 §4): the client-server API over plain HTTPS. What
//! comes in is a `/sync` long-poll (`sync`); what goes out is
//! `PUT /rooms/{room}/send/…`, with an HTML rendering of the Markdown
//! (`html`) beside the plain body.
//!
//! Login is an access token, or a user and password: the password logs in
//! once, and the session is kept in `<data>/gateway/matrix/session.json`
//! (0600) until the server forgets it.
//!
//! Who gets in: a DM (a room of two) from an allow-listed user id; a room
//! on `allowed_rooms` when the bot is mentioned or replied to. Invites are
//! joined only from allowed users. A DM's chat id is the *user* id, so an
//! owner notice or a task's result reaches the person; the room behind it
//! is looked up (or made) when something is sent.
//!
//! No end-to-end encryption: an encrypted room is refused, with one
//! plaintext notice there saying so, and its events are never read.

pub mod html;
mod sync;

use crate::channel::{buttons_as_text, Button, ButtonAction, Channel, ChannelCapabilities};
use crate::channels::access::Access;
use crate::channels::files::{self, Inbox};
use crate::error::GatewayError;
use crate::message::{Attachment, InboundMessage, OutboundMessage};
use crate::stream::chunks;
use crate::typing::Typing;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

/// One event's text: the spec caps an event at 64 KiB, HTML included.
pub const MESSAGE_LIMIT: usize = 16_000;
const REQUEST_DEADLINE: Duration = Duration::from_secs(30);
const CONNECT_DEADLINE: Duration = Duration::from_secs(10);
/// Our own events remembered, so a reply to one counts as a mention.
const SENT: usize = 500;
/// Approval messages remembered with their reactions.
const APPROVALS: usize = 100;

/// Waits and retries, shortened in tests.
#[derive(Clone)]
struct Timing {
    /// `/sync`'s `timeout`: how long the server holds an empty answer.
    sync: Duration,
    backoff_min: Duration,
    backoff_max: Duration,
    /// The longest a 429 is waited out before a send gives up.
    rate_cap: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            sync: Duration::from_secs(30),
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            rate_cap: Duration::from_secs(30),
        }
    }
}

/// How the bot logs in.
#[derive(Clone)]
pub enum Login {
    Token(String),
    Password { user: String, password: String },
}

impl std::fmt::Debug for Login {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Login::Token(_) => f.write_str("Token(…)"),
            Login::Password { user, .. } => write!(f, "Password {{ user: {user:?}, .. }}"),
        }
    }
}

/// Everything the adapter needs.
#[derive(Clone, Debug)]
pub struct MatrixConfig {
    /// `https://matrix.example.org` (the client API's base, not the
    /// server name).
    pub homeserver: String,
    pub login: Login,
    /// `<data>/gateway/matrix`: the sync position, DM rooms, encrypted
    /// rooms, a password login's session.
    pub state_dir: Option<PathBuf>,
    /// Where files people send are saved; `None`: not saved.
    pub inbox: Option<Inbox>,
}

/// A client-server API error.
#[derive(Debug, Clone)]
struct MxError {
    status: u16,
    errcode: String,
    message: String,
    retry_after: Option<Duration>,
}

impl MxError {
    fn transport(what: &str, e: reqwest::Error) -> Self {
        Self {
            status: 0,
            errcode: String::new(),
            message: format!("matrix {what} failed: {}", e.without_url()),
            retry_after: None,
        }
    }
}

/// Who the bot is, once logged in.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Session {
    user_id: String,
    access_token: String,
    #[serde(default)]
    device_id: Option<String>,
    /// For `session.json`: which login it belongs to.
    #[serde(default)]
    homeserver: String,
    #[serde(default)]
    user: String,
}

/// What `state.json` keeps across restarts.
#[derive(Default, Serialize, Deserialize)]
struct State {
    /// Whose sync position `next_batch` is.
    user_id: String,
    next_batch: Option<String>,
    /// User id → the DM room with them.
    dms: BTreeMap<String, String>,
    /// Rooms known to be encrypted, and those already told so.
    encrypted: BTreeSet<String>,
    noticed: BTreeSet<String>,
}

/// What is cached about a room.
#[derive(Default, Clone)]
struct Room {
    members: Option<usize>,
    encrypted: Option<bool>,
}

/// An approval's answers: (reaction key, command).
type Choices = Vec<(String, String)>;

pub struct MatrixChannel {
    cfg: MatrixConfig,
    base: String,
    client: reqwest::Client,
    access: Access,
    /// Setup's pairing: any invite is joined.
    pairing: bool,
    timing: Timing,
    session: tokio::sync::Mutex<Option<Session>>,
    /// The bot's own user id and display name, once known.
    me: Mutex<(String, String)>,
    state: Mutex<State>,
    rooms: Mutex<HashMap<String, Room>>,
    sent: Mutex<VecDeque<String>>,
    /// Approval message → (reaction key, command).
    approvals: Mutex<VecDeque<(String, Choices)>>,
    txn: AtomicU64,
    txn_base: String,
    last_poll: Mutex<Option<SystemTime>>,
    /// The login or token was refused.
    auth_problem: Mutex<Option<String>>,
    /// `/sync` is failing.
    sync_problem: Mutex<Option<String>>,
    /// This one runs `/sync` (the gateway); `ferrule tasks run-now` only
    /// sends, and never moves the sync position.
    syncing: AtomicBool,
    /// Rooms left or kicked from: not DMs any more, whatever the file says.
    left: Mutex<BTreeSet<String>>,
}

impl MatrixChannel {
    pub fn new(cfg: MatrixConfig) -> Self {
        let base = cfg.homeserver.trim_end_matches('/').to_string();
        let state = cfg
            .state_dir
            .as_deref()
            .and_then(|d| std::fs::read_to_string(d.join("state.json")).ok())
            .and_then(|s| serde_json::from_str::<State>(&s).ok())
            .unwrap_or_default();
        let rooms = state
            .encrypted
            .iter()
            .map(|r| {
                (
                    r.clone(),
                    Room {
                        members: None,
                        encrypted: Some(true),
                    },
                )
            })
            .collect();
        Self {
            client: crate::channels::ws::http_client(&base, CONNECT_DEADLINE, REQUEST_DEADLINE),
            base,
            access: Self::access(vec![], vec![]),
            pairing: false,
            timing: Timing::default(),
            session: tokio::sync::Mutex::new(None),
            me: Mutex::new((String::new(), String::new())),
            state: Mutex::new(state),
            rooms: Mutex::new(rooms),
            sent: Mutex::new(VecDeque::new()),
            approvals: Mutex::new(VecDeque::new()),
            txn: AtomicU64::new(0),
            txn_base: uuid::Uuid::new_v4().simple().to_string()[..12].to_string(),
            last_poll: Mutex::new(None),
            auth_problem: Mutex::new(None),
            sync_problem: Mutex::new(None),
            syncing: AtomicBool::new(false),
            left: Mutex::new(BTreeSet::new()),
            cfg,
        }
    }

    fn access(users: Vec<String>, rooms: Vec<String>) -> Access {
        Access::new("matrix", "Matrix user id", users, rooms).with_keys(
            "[gateway.matrix] allowed_users",
            "[gateway.matrix] allowed_rooms",
        )
    }

    /// Who may talk to the bot (`@max:example.org`), and the rooms it
    /// answers in when mentioned (`!abc:example.org`).
    pub fn with_allowed(mut self, users: Vec<String>, rooms: Vec<String>) -> Self {
        self.access = Self::access(users, rooms);
        self
    }

    /// Setup only: invites are joined, and the first DM that is exactly
    /// `code` pairs its author.
    pub fn with_pairing(mut self, code: impl Into<String>) -> Self {
        let access = std::mem::replace(&mut self.access, Self::access(vec![], vec![]));
        self.access = access.with_pairing(code);
        self.pairing = true;
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
            sync: Duration::from_millis(200),
            backoff_min: Duration::from_millis(20),
            backoff_max: Duration::from_millis(100),
            rate_cap: Duration::from_millis(100),
        };
        self
    }

    /// Rooms refused as encrypted.
    pub fn encrypted_rooms(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .encrypted
            .iter()
            .cloned()
            .collect()
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Sends `req`; the response when it succeeded, else the server's
    /// error. Never says the token.
    async fn http(req: reqwest::RequestBuilder, what: &str) -> Result<reqwest::Response, MxError> {
        let resp = req.send().await.map_err(|e| MxError::transport(what, e))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let text = resp.text().await.unwrap_or_default();
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let errcode = v["errcode"].as_str().unwrap_or("").to_string();
        let detail = v["error"]
            .as_str()
            .map(|m| crate::health::clip(m, 300))
            .unwrap_or_else(|| format!("status {status}"));
        Err(MxError {
            status: status.as_u16(),
            message: if errcode.is_empty() {
                format!("matrix {what} failed: {detail}")
            } else {
                format!("matrix {what} failed ({errcode}): {detail}")
            },
            errcode,
            retry_after: v["retry_after_ms"].as_u64().map(Duration::from_millis),
        })
    }

    async fn json_of(resp: reqwest::Response, what: &str) -> Result<Value, MxError> {
        resp.json().await.map_err(|e| MxError::transport(what, e))
    }

    /// The access token, logging in first when there is none.
    async fn token(&self) -> Result<String, MxError> {
        let mut session = self.session.lock().await;
        if let Some(s) = &*session {
            return Ok(s.access_token.clone());
        }
        let s = self.login().await.inspect_err(|e| {
            *self.auth_problem.lock().unwrap() = Some(self.refused(e));
        })?;
        *self.auth_problem.lock().unwrap() = None;
        let token = s.access_token.clone();
        *self.me.lock().unwrap() = (s.user_id.clone(), localpart(&s.user_id).to_string());
        let me = s.user_id.clone();
        *session = Some(s);
        drop(session);
        self.fetch_display_name(&me, &token).await;
        Ok(token)
    }

    /// The login's failure, in words the owner can act on.
    fn refused(&self, e: &MxError) -> String {
        match (&self.cfg.login, e.errcode.as_str()) {
            (Login::Token(_), "M_UNKNOWN_TOKEN" | "M_MISSING_TOKEN") => format!(
                "the Matrix access token was refused (logged out or revoked?): {}. Log in again with `ferrule setup` → Matrix",
                e.message
            ),
            (Login::Password { .. }, "M_FORBIDDEN") => format!(
                "Matrix refused the user or password: {}. Check [gateway.matrix] user and the password, or run `ferrule setup` → Matrix",
                e.message
            ),
            _ => e.message.clone(),
        }
    }

    /// A token's `whoami`, or a password's login (or its kept session).
    async fn login(&self) -> Result<Session, MxError> {
        match &self.cfg.login {
            Login::Token(token) => {
                let user_id = whoami(&self.client, &self.base, token).await?;
                Ok(Session {
                    user_id,
                    access_token: token.clone(),
                    device_id: None,
                    homeserver: self.base.clone(),
                    user: String::new(),
                })
            }
            Login::Password { user, password } => {
                if let Some(s) = self.kept_session(user) {
                    return Ok(s);
                }
                let s = password_login(&self.client, &self.base, user, password).await?;
                self.keep_session(&s);
                Ok(s)
            }
        }
    }

    fn kept_session(&self, user: &str) -> Option<Session> {
        let dir = self.cfg.state_dir.as_deref()?;
        let s: Session =
            serde_json::from_str(&std::fs::read_to_string(dir.join("session.json")).ok()?).ok()?;
        (s.homeserver == self.base && s.user == user).then_some(s)
    }

    fn keep_session(&self, s: &Session) {
        if let Some(dir) = &self.cfg.state_dir {
            write_json(dir, "session.json", s, true);
        }
    }

    /// The server forgot the token: a password logs in again once; a
    /// token is a problem to report.
    async fn token_refused(&self, stale: &str) -> bool {
        let mut session = self.session.lock().await;
        if session.as_ref().is_some_and(|s| s.access_token != stale) {
            // Someone else already logged in again.
            return true;
        }
        *session = None;
        match &self.cfg.login {
            Login::Password { .. } => {
                if let Some(dir) = &self.cfg.state_dir {
                    let _ = std::fs::remove_file(dir.join("session.json"));
                }
                true
            }
            Login::Token(_) => false,
        }
    }

    /// Best effort: the name people see, for mentions.
    async fn fetch_display_name(&self, me: &str, token: &str) {
        let url = self.url(&format!(
            "/_matrix/client/v3/profile/{}/displayname",
            enc(me)
        ));
        let req = self.client.get(url).bearer_auth(token);
        let v = match Self::http(req, "profile").await {
            Ok(resp) => Self::json_of(resp, "profile").await,
            Err(e) => Err(e),
        };
        if let Ok(v) = v {
            if let Some(name) = v["displayname"].as_str().filter(|n| !n.trim().is_empty()) {
                self.me.lock().unwrap().1 = name.trim().to_string();
            }
        }
    }

    /// An authenticated request built by `build(token)`, logging in again
    /// once when the server forgot the session.
    async fn call_with(
        &self,
        build: impl Fn(&str) -> reqwest::RequestBuilder,
        what: &str,
    ) -> Result<reqwest::Response, MxError> {
        let mut again = true;
        loop {
            let token = self.token().await?;
            match Self::http(build(&token).bearer_auth(&token), what).await {
                Err(e) if e.errcode == "M_UNKNOWN_TOKEN" => {
                    if again && self.token_refused(&token).await {
                        again = false;
                        continue;
                    }
                    *self.auth_problem.lock().unwrap() = Some(self.refused(&e));
                    return Err(e);
                }
                r => return r,
            }
        }
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        what: &str,
    ) -> Result<Value, MxError> {
        let url = self.url(path);
        let resp = self
            .call_with(
                |_| {
                    let req = self.client.request(method.clone(), &url);
                    match body {
                        Some(b) => req.json(b),
                        None => req,
                    }
                },
                what,
            )
            .await?;
        Self::json_of(resp, what).await
    }

    /// Our user id (logging in if needed).
    async fn me(&self) -> Result<String, MxError> {
        self.token().await?;
        Ok(self.me.lock().unwrap().0.clone())
    }

    fn remember_sent(&self, id: &str) {
        let mut sent = self.sent.lock().unwrap();
        sent.push_back(id.to_string());
        if sent.len() > SENT {
            sent.pop_front();
        }
    }

    fn is_ours(&self, id: &str) -> bool {
        self.sent.lock().unwrap().iter().any(|s| s == id)
    }

    /// `state.json` as it is on disk now.
    fn disk_state(&self) -> Option<State> {
        let dir = self.cfg.state_dir.as_deref()?;
        let s = std::fs::read_to_string(dir.join("state.json")).ok()?;
        serde_json::from_str(&s).ok()
    }

    /// Writes `state.json`, over what another process (the gateway, or a
    /// `ferrule tasks run-now` beside it) may have written since: its DMs
    /// and encrypted rooms are kept, and only the syncing one moves the
    /// sync position.
    fn save_state(&self) {
        let Some(dir) = &self.cfg.state_dir else {
            return;
        };
        let disk = self.disk_state();
        let left = self.left.lock().unwrap().clone();
        let mut state = self.state.lock().unwrap();
        if let Some(disk) = disk.filter(|d| d.user_id == state.user_id || state.user_id.is_empty())
        {
            if !self.syncing.load(Ordering::Relaxed) {
                state.user_id = disk.user_id;
                state.next_batch = disk.next_batch;
            }
            for (user, room) in disk.dms {
                if !left.contains(&room) {
                    state.dms.entry(user).or_insert(room);
                }
            }
            state.encrypted.extend(disk.encrypted);
            state.noticed.extend(disk.noticed);
        }
        write_json(dir, "state.json", &*state, false);
    }

    /// Whether `room` has two members (a DM), asked once and cached.
    async fn is_dm(&self, room: &str) -> bool {
        if let Some(n) = self.rooms.lock().unwrap().get(room).and_then(|r| r.members) {
            return n == 2;
        }
        let path = format!("/_matrix/client/v3/rooms/{}/joined_members", enc(room));
        match self.call(Method::GET, &path, None, "members").await {
            Ok(v) => {
                let n = v["joined"].as_object().map_or(0, |m| m.len());
                self.rooms
                    .lock()
                    .unwrap()
                    .entry(room.to_string())
                    .or_default()
                    .members = Some(n);
                n == 2
            }
            Err(e) => {
                tracing::warn!("matrix: {}", e.message);
                false
            }
        }
    }

    /// Whether `room` is end-to-end encrypted, asked once and cached.
    async fn is_encrypted(&self, room: &str) -> bool {
        if let Some(e) = self
            .rooms
            .lock()
            .unwrap()
            .get(room)
            .and_then(|r| r.encrypted)
        {
            return e;
        }
        let path = format!(
            "/_matrix/client/v3/rooms/{}/state/m.room.encryption/",
            enc(room)
        );
        let known = match self.call(Method::GET, &path, None, "room state").await {
            Ok(_) => Some(true),
            Err(e) if e.status == 404 => Some(false),
            Err(e) => {
                tracing::warn!("matrix: {}", e.message);
                None
            }
        };
        match known {
            Some(true) => {
                self.mark_encrypted(room);
                true
            }
            Some(false) => {
                self.rooms
                    .lock()
                    .unwrap()
                    .entry(room.to_string())
                    .or_default()
                    .encrypted = Some(false);
                false
            }
            None => false,
        }
    }

    fn mark_encrypted(&self, room: &str) {
        self.rooms
            .lock()
            .unwrap()
            .entry(room.to_string())
            .or_default()
            .encrypted = Some(true);
        let new = self
            .state
            .lock()
            .unwrap()
            .encrypted
            .insert(room.to_string());
        if new {
            tracing::warn!("matrix: {room} is end-to-end encrypted; ferrule refuses it");
            self.save_state();
        }
    }

    /// Tells an encrypted room, once, why nothing will be answered there.
    async fn notice_encrypted(&self, room: &str) {
        let first = self.state.lock().unwrap().noticed.insert(room.to_string());
        if !first {
            return;
        }
        self.save_state();
        let text = "This room is end-to-end encrypted, and ferrule can't read encrypted messages, \
                    so it won't answer here. Create a new room with encryption turned off \
                    (Element: New room → turn off \"Enable end-to-end encryption\") and invite me there.";
        let content = json!({ "msgtype": "m.notice", "body": text });
        if let Err(e) = self.send_event(room, "m.room.message", &content).await {
            tracing::warn!("matrix: couldn't post the encryption notice: {}", e.message);
        }
    }

    /// The room behind a chat id: a room id as is, an alias looked up, a
    /// user id's DM (made when there is none).
    async fn room_for(&self, chat: &str) -> Result<String, GatewayError> {
        match chat.chars().next() {
            Some('!') => Ok(chat.to_string()),
            Some('#') => {
                let path = format!("/_matrix/client/v3/directory/room/{}", enc(chat));
                let v = self
                    .call(Method::GET, &path, None, "alias lookup")
                    .await
                    .map_err(|e| GatewayError::Channel(e.message))?;
                v["room_id"]
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| GatewayError::Channel(format!("matrix: {chat} has no room")))
            }
            Some('@') => {
                if let Some(room) = self.state.lock().unwrap().dms.get(chat).cloned() {
                    return Ok(room);
                }
                // A DM `ferrule tasks run-now` opened meanwhile.
                let left = self.left.lock().unwrap().clone();
                if let Some(room) = self
                    .disk_state()
                    .and_then(|d| d.dms.get(chat).cloned())
                    .filter(|r| !left.contains(r))
                {
                    self.state
                        .lock()
                        .unwrap()
                        .dms
                        .insert(chat.to_string(), room.clone());
                    return Ok(room);
                }
                self.create_dm(chat).await
            }
            _ => Err(GatewayError::Channel(format!(
                "matrix: {chat:?} isn't a room id (!…), alias (#…) or user id (@…)"
            ))),
        }
    }

    /// Opens a DM with `user`: a private room, unencrypted, inviting them.
    async fn create_dm(&self, user: &str) -> Result<String, GatewayError> {
        let body = json!({
            "preset": "trusted_private_chat",
            "is_direct": true,
            "invite": [user],
        });
        let v = self
            .call(
                Method::POST,
                "/_matrix/client/v3/createRoom",
                Some(&body),
                "createRoom",
            )
            .await
            .map_err(|e| GatewayError::Channel(e.message))?;
        let room = v["room_id"]
            .as_str()
            .ok_or_else(|| GatewayError::Channel("matrix createRoom gave no room id".into()))?
            .to_string();
        tracing::info!("matrix: opened a DM with {user}: {room}");
        self.remember_dm(user, &room);
        Ok(room)
    }

    fn remember_dm(&self, user: &str, room: &str) {
        self.left.lock().unwrap().remove(room);
        let changed = {
            let mut state = self.state.lock().unwrap();
            state
                .dms
                .insert(user.to_string(), room.to_string())
                .as_deref()
                != Some(room)
        };
        if changed {
            self.save_state();
        }
    }

    /// `PUT /rooms/{room}/send/{type}/{txn}`; the event id. A 429 is
    /// waited out (the server's `retry_after_ms`) three times.
    async fn send_event(&self, room: &str, kind: &str, content: &Value) -> Result<String, MxError> {
        let txn = format!(
            "ferrule-{}-{}",
            self.txn_base,
            self.txn.fetch_add(1, Ordering::Relaxed)
        );
        let path = format!(
            "/_matrix/client/v3/rooms/{}/send/{}/{}",
            enc(room),
            enc(kind),
            txn
        );
        let mut tries = 0;
        loop {
            tries += 1;
            match self.call(Method::PUT, &path, Some(content), "send").await {
                Err(e) if e.status == 429 && tries <= 3 => {
                    let wait = e
                        .retry_after
                        .unwrap_or(self.timing.backoff_min)
                        .min(self.timing.rate_cap);
                    tracing::warn!("matrix: rate limited, waiting {wait:?}");
                    tokio::time::sleep(wait).await;
                }
                Err(e) => return Err(e),
                Ok(v) => {
                    let id = v["event_id"].as_str().unwrap_or("").to_string();
                    if !id.is_empty() {
                        self.remember_sent(&id);
                    }
                    return Ok(id);
                }
            }
        }
    }

    /// The error a send returns.
    fn explain(&self, room: &str, e: MxError) -> GatewayError {
        match e.status {
            429 => GatewayError::RateLimited {
                retry_after: e.retry_after.unwrap_or(self.timing.backoff_max),
            },
            403 => GatewayError::Channel(format!(
                "{} (is the bot in {room}, and allowed to post there?)",
                e.message
            )),
            _ => GatewayError::Channel(e.message),
        }
    }

    /// Text, files, a reply: all of `msg`; the last event's id.
    async fn deliver(&self, msg: &OutboundMessage) -> Result<Option<String>, GatewayError> {
        let room = self.room_for(&msg.chat_id).await?;
        if self.is_encrypted(&room).await {
            return Err(GatewayError::Channel(format!(
                "matrix: {room} is end-to-end encrypted, and ferrule doesn't write into encrypted rooms"
            )));
        }
        let mut last = None;
        for att in &msg.attachments {
            let id = self.send_file(&room, att).await?;
            last = Some(id);
        }
        let mut reply = msg.reply_to.clone().filter(|r| r.starts_with('$'));
        for piece in chunks(&msg.text, MESSAGE_LIMIT) {
            if piece.trim().is_empty() {
                continue;
            }
            let mut content = text_content(&piece);
            if let Some(r) = reply.take() {
                content["m.relates_to"] = json!({ "m.in_reply_to": { "event_id": r } });
            }
            let id = self
                .send_event(&room, "m.room.message", &content)
                .await
                .map_err(|e| self.explain(&room, e))?;
            last = Some(id);
        }
        Ok(last)
    }

    /// Uploads a local file and posts it; the event id.
    async fn send_file(&self, room: &str, att: &Attachment) -> Result<String, GatewayError> {
        let (name, mime, bytes) = files::read_outgoing(att)?;
        let mime = if att.kind.contains('/') {
            att.kind.clone()
        } else {
            mime.to_string()
        };
        let size = bytes.len();
        let url = self.url("/_matrix/media/v3/upload");
        let resp = self
            .call_with(
                |_| {
                    self.client
                        .post(&url)
                        .query(&[("filename", name.as_str())])
                        .header("content-type", mime.as_str())
                        .body(bytes.clone())
                },
                "upload",
            )
            .await
            .map_err(|e| self.explain(room, e))?;
        let v = Self::json_of(resp, "upload")
            .await
            .map_err(|e| GatewayError::Channel(e.message))?;
        let uri = v["content_uri"]
            .as_str()
            .ok_or_else(|| GatewayError::Channel("matrix upload gave no content_uri".into()))?;
        let msgtype = match mime.split('/').next().unwrap_or("") {
            "image" => "m.image",
            "audio" => "m.audio",
            "video" => "m.video",
            _ => "m.file",
        };
        let content = json!({
            "msgtype": msgtype,
            "body": name,
            "filename": name,
            "url": uri,
            "info": { "mimetype": mime, "size": size },
        });
        self.send_event(room, "m.room.message", &content)
            .await
            .map_err(|e| self.explain(room, e))
    }

    /// Downloads an `mxc://` file into the inbox: authenticated media
    /// first, the legacy endpoint when the server is older.
    async fn fetch_media(
        &self,
        mxc: &str,
        name: &str,
        size: Option<u64>,
        mime: Option<&str>,
        event_id: &str,
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
        let Some((server, id)) = mxc
            .strip_prefix("mxc://")
            .and_then(|r| r.split_once('/'))
            .filter(|(s, i)| !s.is_empty() && !i.is_empty())
        else {
            return Err(refuse(format!("{mxc:?} isn't a Matrix media link")));
        };
        let tail = format!("{}/{}", enc(server), enc(id));
        let new = self.url(&format!("/_matrix/client/v1/media/download/{tail}"));
        let resp = match self.call_with(|_| self.client.get(&new), "download").await {
            Err(e) if e.status == 404 || e.errcode == "M_UNRECOGNIZED" => {
                let old = self.url(&format!("/_matrix/media/v3/download/{tail}"));
                self.call_with(|_| self.client.get(&old), "download").await
            }
            r => r,
        }
        .map_err(|e| refuse(e.message))?;
        let bytes = files::read_capped(resp, inbox.max_bytes())
            .await
            .map_err(refuse)?;
        inbox
            .save("matrix", event_id, name, mime, &bytes)
            .map_err(|e| refuse(format!("it couldn't be saved: {e}")))
    }

    /// An approval: the answers listed as text, and one reaction per
    /// answer on the message, so a tap sends it.
    async fn deliver_buttons(
        &self,
        msg: &OutboundMessage,
        buttons: &[Button],
    ) -> Result<(), GatewayError> {
        let mut keyed = vec![];
        let mut digit = 0;
        for b in buttons {
            if let ButtonAction::Command(cmd) = &b.action {
                let key = if cmd.starts_with("yes ") && !keyed.iter().any(|(k, _, _)| k == "👍") {
                    "👍".to_string()
                } else if cmd.starts_with("no ") && !keyed.iter().any(|(k, _, _)| k == "👎") {
                    "👎".to_string()
                } else {
                    digit += 1;
                    if digit > 9 {
                        continue;
                    }
                    format!("{digit}\u{fe0f}\u{20e3}")
                };
                keyed.push((key, b.text.clone(), cmd.clone()));
            }
        }
        let mut text = msg.text.clone();
        for (key, label, cmd) in &keyed {
            text.push_str(&format!("\n• {key} {label}: react {key}, or send `{cmd}`"));
        }
        let links: Vec<Button> = buttons
            .iter()
            .filter(|b| matches!(b.action, ButtonAction::Url(_)))
            .cloned()
            .collect();
        let text = buttons_as_text(&text, &links);
        let out = OutboundMessage {
            text,
            ..msg.clone()
        };
        let Some(id) = self.deliver(&out).await? else {
            return Ok(());
        };
        if keyed.is_empty() {
            return Ok(());
        }
        let room = self.room_for(&msg.chat_id).await?;
        {
            let mut approvals = self.approvals.lock().unwrap();
            approvals.push_back((
                id.clone(),
                keyed.iter().map(|(k, _, c)| (bare(k), c.clone())).collect(),
            ));
            if approvals.len() > APPROVALS {
                approvals.pop_front();
            }
        }
        for (key, _, _) in &keyed {
            if let Err(e) = self.annotate(&room, &id, key).await {
                tracing::warn!("matrix: couldn't add the {key} reaction: {}", e.message);
            }
        }
        Ok(())
    }

    /// The command an allowed user's reaction on an approval stands for
    /// (used once).
    fn approval_answer(&self, event: &str, key: &str) -> Option<String> {
        let mut approvals = self.approvals.lock().unwrap();
        let n = approvals.iter().position(|(id, _)| id == event)?;
        let cmd = approvals[n]
            .1
            .iter()
            .find(|(k, _)| *k == bare(key))
            .map(|(_, c)| c.clone())?;
        approvals.remove(n);
        Some(cmd)
    }

    async fn annotate(&self, room: &str, event: &str, key: &str) -> Result<String, MxError> {
        let content = json!({
            "m.relates_to": { "rel_type": "m.annotation", "event_id": event, "key": key },
        });
        self.send_event(room, "m.reaction", &content).await
    }
}

/// A reaction key without variation selectors, so 👍 and 👍️ match.
fn bare(key: &str) -> String {
    key.replace('\u{fe0f}', "")
}

/// The text event for `text`: Markdown as the body, HTML beside it.
fn text_content(text: &str) -> Value {
    json!({
        "msgtype": "m.text",
        "body": text,
        "format": "org.matrix.custom.html",
        "formatted_body": html::to_html(text),
    })
}

/// `@ferrule:example.org` → `ferrule`.
pub fn localpart(user_id: &str) -> &str {
    user_id
        .trim_start_matches('@')
        .split(':')
        .next()
        .unwrap_or(user_id)
}

/// A path segment, percent-encoded (room ids hold `!` and `:`).
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

/// Writes `value` to `<dir>/<name>` through a temporary file; `private`
/// makes it 0600 (a session's token).
fn write_json<T: Serialize>(dir: &Path, name: &str, value: &T, private: bool) {
    let Ok(body) = serde_json::to_string_pretty(value) else {
        return;
    };
    let tmp = dir.join(format!(".{name}.tmp"));
    let r = std::fs::create_dir_all(dir)
        .and_then(|_| {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            if private {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            #[cfg(not(unix))]
            let _ = private;
            use std::io::Write;
            opts.open(&tmp)?.write_all(body.as_bytes())
        })
        .and_then(|_| std::fs::rename(&tmp, dir.join(name)));
    if let Err(e) = r {
        tracing::warn!(error = %e, "matrix: couldn't save {name}");
    }
}

/// Rooms `state.json` lists as encrypted, for doctor.
pub fn encrypted_on_disk(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("state.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<State>(&s).ok())
        .map(|s| s.encrypted.into_iter().collect())
        .unwrap_or_default()
}

async fn whoami(client: &reqwest::Client, base: &str, token: &str) -> Result<String, MxError> {
    let req = client
        .get(format!("{base}/_matrix/client/v3/account/whoami"))
        .bearer_auth(token);
    let v = MatrixChannel::json_of(MatrixChannel::http(req, "whoami").await?, "whoami").await?;
    v["user_id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| MxError {
            status: 0,
            errcode: String::new(),
            message: "matrix whoami gave no user id".into(),
            retry_after: None,
        })
}

async fn password_login(
    client: &reqwest::Client,
    base: &str,
    user: &str,
    password: &str,
) -> Result<Session, MxError> {
    let body = json!({
        "type": "m.login.password",
        "identifier": { "type": "m.id.user", "user": user },
        "password": password,
        "initial_device_display_name": "ferrule",
    });
    let req = client
        .post(format!("{base}/_matrix/client/v3/login"))
        .json(&body);
    let v = MatrixChannel::json_of(MatrixChannel::http(req, "login").await?, "login").await?;
    let (Some(user_id), Some(token)) = (v["user_id"].as_str(), v["access_token"].as_str()) else {
        return Err(MxError {
            status: 0,
            errcode: String::new(),
            message: "matrix login gave no token".into(),
            retry_after: None,
        });
    };
    Ok(Session {
        user_id: user_id.into(),
        access_token: token.into(),
        device_id: v["device_id"].as_str().map(str::to_string),
        homeserver: base.into(),
        user: user.into(),
    })
}

/// The client API's base URL for what the user typed: `matrix.org`,
/// `@me:matrix.org` or a URL. The server's `.well-known` wins when it has
/// one (matrix.org's client API is `matrix-client.matrix.org`).
pub async fn discover(input: &str) -> String {
    let input = input.trim().trim_end_matches('/');
    let host = match input.split_once(':') {
        Some((user, server)) if user.starts_with('@') => server.to_string(),
        _ => input.to_string(),
    };
    let base = if host.starts_with("http://") || host.starts_with("https://") {
        host
    } else {
        format!("https://{host}")
    };
    let client = crate::channels::ws::http_client(&base, CONNECT_DEADLINE, CONNECT_DEADLINE);
    let found = async {
        let v: Value = client
            .get(format!("{base}/.well-known/matrix/client"))
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json()
            .await
            .ok()?;
        v["m.homeserver"]["base_url"]
            .as_str()
            .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
            .map(|u| u.trim_end_matches('/').to_string())
    }
    .await;
    found.unwrap_or(base)
}

/// Logs in with a password once and gives back `(user id, token)`: setup
/// keeps the token, never the password.
pub async fn login_for_token(
    homeserver: &str,
    user: &str,
    password: &str,
) -> Result<(String, String), String> {
    let base = homeserver.trim_end_matches('/');
    let client = crate::channels::ws::http_client(base, CONNECT_DEADLINE, REQUEST_DEADLINE);
    match password_login(&client, base, user, password).await {
        Ok(s) => Ok((s.user_id, s.access_token)),
        Err(e) if e.errcode == "M_FORBIDDEN" => Err(format!(
            "the homeserver refused the user or password ({})",
            e.message
        )),
        Err(e) => Err(e.message),
    }
}

/// What the probe learned about the bot's account.
#[derive(Debug, Clone)]
pub struct Probe {
    pub user_id: String,
    pub name: String,
    pub rooms: usize,
    /// Joined rooms that are encrypted (the first 20 rooms are checked).
    pub encrypted: Vec<String>,
}

impl Probe {
    /// "@bot:server (Ferrule) · 2 rooms, 1 encrypted (refused)".
    pub fn summary(&self) -> String {
        let mut s = self.user_id.clone();
        if !self.name.is_empty() && self.name != localpart(&self.user_id) {
            s.push_str(&format!(" ({})", self.name));
        }
        s.push_str(&format!(
            " · {} room{}",
            self.rooms,
            if self.rooms == 1 { "" } else { "s" }
        ));
        if !self.encrypted.is_empty() {
            s.push_str(&format!(", {} encrypted (refused)", self.encrypted.len()));
        }
        s
    }
}

/// Logs in (or checks the token) and lists the joined rooms: nothing is
/// sent. A password login made only for this is logged out again.
pub async fn probe(cfg: MatrixConfig) -> Result<Probe, String> {
    if !(cfg.homeserver.starts_with("https://") || cfg.homeserver.starts_with("http://")) {
        return Err(format!(
            "the homeserver should be a URL like https://matrix.example.org, not {:?}",
            cfg.homeserver
        ));
    }
    let fresh_password = matches!(cfg.login, Login::Password { .. }) && cfg.state_dir.is_none();
    let ch = MatrixChannel::new(cfg);
    let versions = MatrixChannel::http(
        ch.client.get(ch.url("/_matrix/client/versions")),
        "versions",
    )
    .await;
    if let Err(e) = versions {
        return Err(if e.status == 404 {
            format!(
                "{} doesn't answer as a Matrix homeserver (no /_matrix/client/versions): use the client API's address (for matrix.org, https://matrix-client.matrix.org)",
                ch.base
            )
        } else {
            e.message
        });
    }
    let me = ch.me().await.map_err(|e| ch.refused(&e))?;
    let joined = ch
        .call(
            Method::GET,
            "/_matrix/client/v3/joined_rooms",
            None,
            "joined_rooms",
        )
        .await
        .map_err(|e| e.message)?;
    let rooms: Vec<String> = joined["joined_rooms"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|r| r.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let mut encrypted = vec![];
    for room in rooms.iter().take(20) {
        if ch.is_encrypted(room).await {
            encrypted.push(room.clone());
        }
    }
    let name = ch.me.lock().unwrap().1.clone();
    if fresh_password {
        if let Err(e) = ch
            .call(
                Method::POST,
                "/_matrix/client/v3/logout",
                Some(&json!({})),
                "logout",
            )
            .await
        {
            tracing::debug!("matrix: probe logout: {}", e.message);
        }
    }
    Ok(Probe {
        user_id: me,
        name,
        rooms: rooms.len(),
        encrypted,
    })
}

#[async_trait::async_trait]
impl Channel for MatrixChannel {
    fn name(&self) -> &str {
        "matrix"
    }

    /// `PUT /rooms/{room}/typing/{me}` for 30 s, again every 25; cleared
    /// at the end.
    async fn typing(&self, chat_id: &str, _message_id: &str, on: bool) -> Typing {
        let (Ok(room), Ok(me)) = (self.room_for(chat_id).await, self.me().await) else {
            return Typing::Failed;
        };
        let path = format!(
            "/_matrix/client/v3/rooms/{}/typing/{}",
            enc(&room),
            enc(&me)
        );
        let body = if on {
            json!({ "typing": true, "timeout": 30000 })
        } else {
            json!({ "typing": false })
        };
        match self.call(Method::PUT, &path, Some(&body), "typing").await {
            Ok(_) => Typing::Shown {
                again_in: Duration::from_secs(25),
            },
            Err(e) if e.status == 429 => Typing::Limited,
            Err(e) => {
                tracing::debug!("matrix: typing: {}", e.message);
                Typing::Failed
            }
        }
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
            .or_else(|| self.sync_problem.lock().unwrap().clone())
    }

    fn message_limit(&self) -> Option<usize> {
        Some(MESSAGE_LIMIT)
    }

    /// Homeservers limit messages per second, and an edit is a message:
    /// a streamed answer is edited every 3 s at most.
    fn stream_every(&self) -> Option<Duration> {
        Some(Duration::from_secs(3))
    }

    async fn run(&self, tx: tokio::sync::mpsc::Sender<InboundMessage>) -> Result<(), GatewayError> {
        self.run_sync(tx).await
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

    /// The 👀: a read receipt, and the reaction.
    async fn react(
        &self,
        chat_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<(), GatewayError> {
        if !message_id.starts_with('$') {
            return Ok(());
        }
        let room = self.room_for(chat_id).await?;
        let path = format!(
            "/_matrix/client/v3/rooms/{}/receipt/m.read/{}",
            enc(&room),
            enc(message_id)
        );
        if let Err(e) = self
            .call(Method::POST, &path, Some(&json!({})), "receipt")
            .await
        {
            tracing::debug!("matrix: read receipt: {}", e.message);
        }
        self.annotate(&room, message_id, emoji)
            .await
            .map(|_| ())
            .map_err(|e| self.explain(&room, e))
    }

    async fn edit(&self, chat_id: &str, message_id: &str, text: &str) -> Result<(), GatewayError> {
        let room = self.room_for(chat_id).await?;
        let text = chunks(text, MESSAGE_LIMIT)
            .into_iter()
            .next()
            .unwrap_or_default();
        // Clients without edits show the fallback: the text, starred.
        let mut content = json!({
            "msgtype": "m.text",
            "body": format!("* {text}"),
            "format": "org.matrix.custom.html",
            "formatted_body": format!("* {}", html::to_html(&text)),
        });
        content["m.new_content"] = text_content(&text);
        content["m.relates_to"] = json!({ "rel_type": "m.replace", "event_id": message_id });
        self.send_event(&room, "m.room.message", &content)
            .await
            .map(|_| ())
            .map_err(|e| self.explain(&room, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_encode_and_split() {
        assert_eq!(enc("!abc:example.org"), "%21abc%3Aexample.org");
        assert_eq!(enc("$ev/1+2"), "%24ev%2F1%2B2");
        assert_eq!(localpart("@ferrule:example.org"), "ferrule");
        assert_eq!(bare("👍\u{fe0f}"), "👍");
    }

    #[test]
    fn a_probe_summary_says_encrypted_rooms() {
        let p = Probe {
            user_id: "@bot:x.org".into(),
            name: "Ferrule".into(),
            rooms: 2,
            encrypted: vec!["!a:x.org".into()],
        };
        assert_eq!(
            p.summary(),
            "@bot:x.org (Ferrule) · 2 rooms, 1 encrypted (refused)"
        );
        let p = Probe {
            user_id: "@bot:x.org".into(),
            name: "bot".into(),
            rooms: 1,
            encrypted: vec![],
        };
        assert_eq!(p.summary(), "@bot:x.org · 1 room");
    }

    #[test]
    fn login_debug_hides_secrets() {
        let l = Login::Password {
            user: "bot".into(),
            password: "hunter2".into(),
        };
        assert!(!format!("{l:?}").contains("hunter2"));
        assert!(!format!("{:?}", Login::Token("syt_secret".into())).contains("syt_"));
    }
}
