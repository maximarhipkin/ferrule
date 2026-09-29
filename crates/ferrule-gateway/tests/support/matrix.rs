//! M39: a mock Matrix homeserver (client-server API): login, whoami,
//! a scripted `/sync`, joins, members, room state, sends, receipts,
//! createRoom, media upload and both download endpoints.

use super::http::{serve, Request, Response};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Mutex};

pub const TOKEN: &str = "syt_mock_token";
pub const BOT: &str = "@ferrule:mock.org";
pub const USER: &str = "ferrule";
pub const PASSWORD: &str = "correct horse";
pub const MAX: &str = "@max:mock.org";
pub const STRANGER: &str = "@eve:mock.org";
/// Max's DM with the bot, and a shared room.
pub const DM: &str = "!dm:mock.org";
pub const ROOM: &str = "!room:mock.org";

#[derive(Default)]
pub struct State {
    /// Access tokens the server accepts.
    pub tokens: BTreeSet<String>,
    pub logins: usize,
    pub logouts: usize,
    /// What a sync without `since` answers (the catch-up).
    pub backlog: Option<Value>,
    /// What the next syncs answer, one each.
    pub syncs: VecDeque<Value>,
    /// `since` of every sync.
    pub sinces: Vec<Option<String>>,
    /// Joined members of each room.
    pub members: HashMap<String, Vec<String>>,
    pub encrypted: BTreeSet<String>,
    /// (room, type, content) of every send.
    pub sent: Vec<(String, String, Value)>,
    /// Txn ids seen.
    pub txns: Vec<String>,
    /// (status, errcode, retry_after_ms) the next sends answer with.
    pub fail_next: VecDeque<(u16, String, u64)>,
    pub joins: Vec<String>,
    pub receipts: Vec<(String, String)>,
    pub created: Vec<Value>,
    /// (filename, content type, bytes).
    pub uploads: Vec<(String, String, Vec<u8>)>,
    /// Paths of downloads.
    pub downloads: Vec<String>,
    /// The authenticated media endpoint is missing (an older server).
    pub v1_missing: bool,
    pub media: BTreeMap<String, String>,
    pub aliases: BTreeMap<String, String>,
    pub unauthorized: usize,
    next: u64,
}

pub struct Homeserver {
    pub url: String,
    pub state: Arc<Mutex<State>>,
}

impl Homeserver {
    pub fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// Queues one sync answer with `events` in `room`'s timeline.
    pub fn timeline(&self, room: &str, events: Vec<Value>) {
        self.push(json!({ "rooms": { "join": { room: { "timeline": { "events": events } } } } }));
    }

    /// Queues one sync answer.
    pub fn push(&self, mut v: Value) {
        let mut s = self.state();
        s.next += 1;
        v["next_batch"] = json!(format!("batch{}", s.next));
        s.syncs.push_back(v);
    }

    /// Queues an invite to `room` from `inviter`.
    pub fn invite(&self, room: &str, inviter: &str, direct: bool, encrypted: bool) {
        let mut events = vec![json!({
            "type": "m.room.member", "state_key": BOT, "sender": inviter,
            "content": { "membership": "invite", "is_direct": direct },
        })];
        if encrypted {
            events.push(json!({
                "type": "m.room.encryption", "state_key": "", "sender": inviter,
                "content": { "algorithm": "m.megolm.v1.aes-sha2" },
            }));
            self.state().encrypted.insert(room.into());
        }
        self.state()
            .members
            .insert(room.into(), vec![inviter.to_string()]);
        self.push(
            json!({ "rooms": { "invite": { room: { "invite_state": { "events": events } } } } }),
        );
    }

    /// The contents sent into `room` of `kind`.
    pub fn sent_to(&self, room: &str, kind: &str) -> Vec<Value> {
        self.state()
            .sent
            .iter()
            .filter(|(r, t, _)| r == room && t == kind)
            .map(|(_, _, c)| c.clone())
            .collect()
    }

    /// The bodies of the messages sent into `room`.
    pub fn bodies(&self, room: &str) -> Vec<String> {
        self.sent_to(room, "m.room.message")
            .iter()
            .filter_map(|c| c["body"].as_str().map(str::to_string))
            .collect()
    }

    pub fn revoke(&self, token: &str) {
        self.state().tokens.remove(token);
    }
}

/// A text message event.
pub fn text(id: &str, sender: &str, body: &str) -> Value {
    json!({
        "type": "m.room.message", "event_id": id, "sender": sender,
        "origin_server_ts": 1_760_000_000_000i64,
        "content": { "msgtype": "m.text", "body": body },
    })
}

fn error(status: u16, code: &str, msg: &str) -> Response {
    Response::json(json!({ "errcode": code, "error": msg })).with_status(status)
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query(path: &str) -> HashMap<String, String> {
    path.split_once('?')
        .map(|(_, q)| q)
        .unwrap_or("")
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (decode(k), decode(&v.replace('+', " "))))
        .collect()
}

/// Starts the homeserver: Max's DM and a shared room exist, the bot in
/// both.
pub fn start() -> Homeserver {
    let mut state = State::default();
    state.tokens.insert(TOKEN.into());
    state
        .members
        .insert(DM.into(), vec![BOT.into(), MAX.into()]);
    state
        .members
        .insert(ROOM.into(), vec![BOT.into(), MAX.into(), STRANGER.into()]);
    state
        .media
        .insert("mock.org/img1".into(), "PNGBYTES".into());
    state
        .aliases
        .insert("#general:mock.org".into(), ROOM.into());
    let state = Arc::new(Mutex::new(state));
    let st = state.clone();
    let port_cell: Arc<Mutex<u16>> = Arc::new(Mutex::new(0));
    let pc = port_cell.clone();
    let port = serve(Arc::new(move |req: Request| {
        handle(&st, *pc.lock().unwrap(), req)
    }));
    *port_cell.lock().unwrap() = port;
    Homeserver {
        url: format!("http://127.0.0.1:{port}"),
        state,
    }
}

fn handle(st: &Mutex<State>, port: u16, req: Request) -> Response {
    let raw = req.path.split('?').next().unwrap_or("").to_string();
    let seg: Vec<String> = raw.split('/').map(decode).collect();
    let seg: Vec<&str> = seg.iter().map(String::as_str).collect();
    let q = query(&req.path);
    match (req.method.as_str(), &seg[1..]) {
        ("GET", [".well-known", "matrix", "client"]) => {
            return Response::json(json!({
                "m.homeserver": { "base_url": format!("http://127.0.0.1:{port}") },
            }))
        }
        ("GET", ["_matrix", "client", "versions"]) => {
            return Response::json(json!({ "versions": ["v1.11"] }))
        }
        ("POST", ["_matrix", "client", "v3", "login"]) => {
            let v = req.json();
            if v["identifier"]["user"] != USER || v["password"] != PASSWORD {
                return error(403, "M_FORBIDDEN", "Invalid username or password");
            }
            let mut s = st.lock().unwrap();
            s.logins += 1;
            let token = format!("syt_login_{}", s.logins);
            s.tokens.insert(token.clone());
            return Response::json(json!({
                "user_id": BOT, "access_token": token, "device_id": "DEV1",
            }));
        }
        _ if seg.get(1) != Some(&"_matrix") => return error(404, "M_UNRECOGNIZED", "Not found"),
        _ => {}
    }
    let token = req
        .header("authorization")
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_string();
    let mut s = st.lock().unwrap();
    if !s.tokens.contains(&token) {
        s.unauthorized += 1;
        return error(401, "M_UNKNOWN_TOKEN", "Invalid access token passed.");
    }
    match (req.method.as_str(), &seg[1..]) {
        ("GET", ["_matrix", "client", "v3", "account", "whoami"]) => {
            Response::json(json!({ "user_id": BOT }))
        }
        ("POST", ["_matrix", "client", "v3", "logout"]) => {
            s.logouts += 1;
            s.tokens.remove(&token);
            Response::json(json!({}))
        }
        ("GET", ["_matrix", "client", "v3", "profile", _, "displayname"]) => {
            Response::json(json!({ "displayname": "Ferrule" }))
        }
        ("GET", ["_matrix", "client", "v3", "sync"]) => {
            let since = q.get("since").cloned();
            s.sinces.push(since.clone());
            if since.is_none() {
                let mut b = s.backlog.clone().unwrap_or(json!({}));
                b["next_batch"] = json!("batch0");
                return Response::json(b);
            }
            if let Some(v) = s.syncs.pop_front() {
                return Response::json(v);
            }
            drop(s);
            // A short long-poll: nothing new.
            std::thread::sleep(std::time::Duration::from_millis(20));
            Response::json(json!({ "next_batch": since.unwrap_or_default() }))
        }
        ("GET", ["_matrix", "client", "v3", "joined_rooms"]) => {
            let rooms: Vec<&String> = s
                .members
                .iter()
                .filter(|(_, m)| m.iter().any(|u| u == BOT))
                .map(|(r, _)| r)
                .collect();
            Response::json(json!({ "joined_rooms": rooms }))
        }
        ("POST", ["_matrix", "client", "v3", "join", room]) => {
            s.joins.push(room.to_string());
            s.members
                .entry(room.to_string())
                .or_default()
                .push(BOT.into());
            Response::json(json!({ "room_id": room }))
        }
        ("GET", ["_matrix", "client", "v3", "directory", "room", alias]) => {
            match s.aliases.get(*alias) {
                Some(r) => Response::json(json!({ "room_id": r, "servers": ["mock.org"] })),
                None => error(404, "M_NOT_FOUND", "Room alias not found"),
            }
        }
        ("POST", ["_matrix", "client", "v3", "createRoom"]) => {
            let v = req.json();
            s.created.push(v.clone());
            let room = format!("!new{}:mock.org", s.created.len());
            let mut members = vec![BOT.to_string()];
            members.extend(
                v["invite"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|u| u.as_str().map(str::to_string)),
            );
            s.members.insert(room.clone(), members);
            Response::json(json!({ "room_id": room }))
        }
        ("GET", ["_matrix", "client", "v3", "rooms", room, "joined_members"]) => {
            let joined: serde_json::Map<String, Value> = s
                .members
                .get(*room)
                .into_iter()
                .flatten()
                .map(|u| (u.clone(), json!({})))
                .collect();
            Response::json(json!({ "joined": joined }))
        }
        ("GET", ["_matrix", "client", "v3", "rooms", room, "state", "m.room.encryption", ""]) => {
            if s.encrypted.contains(*room) {
                Response::json(json!({ "algorithm": "m.megolm.v1.aes-sha2" }))
            } else {
                error(404, "M_NOT_FOUND", "Event not found.")
            }
        }
        ("PUT", ["_matrix", "client", "v3", "rooms", room, "send", kind, txn]) => {
            if let Some((status, code, after)) = s.fail_next.pop_front() {
                return Response::json(json!({
                    "errcode": code, "error": "mock failure", "retry_after_ms": after,
                }))
                .with_status(status);
            }
            s.txns.push(txn.to_string());
            s.sent
                .push((room.to_string(), kind.to_string(), req.json()));
            s.next += 1;
            Response::json(json!({ "event_id": format!("$out{}", s.next) }))
        }
        ("POST", ["_matrix", "client", "v3", "rooms", room, "receipt", "m.read", event]) => {
            s.receipts.push((room.to_string(), event.to_string()));
            Response::json(json!({}))
        }
        ("POST", ["_matrix", "media", "v3", "upload"]) => {
            s.uploads.push((
                q.get("filename").cloned().unwrap_or_default(),
                req.header("content-type").unwrap_or("").to_string(),
                req.body.clone(),
            ));
            Response::json(
                json!({ "content_uri": format!("mxc://mock.org/up{}", s.uploads.len()) }),
            )
        }
        ("GET", ["_matrix", "client", "v1", "media", "download", server, id]) if !s.v1_missing => {
            s.downloads.push(raw.clone());
            match s.media.get(&format!("{server}/{id}")) {
                Some(b) => Response::json(json!(null)).with_body(b),
                None => error(404, "M_NOT_FOUND", "no such media"),
            }
        }
        ("GET", ["_matrix", "media", "v3", "download", server, id]) => {
            s.downloads.push(raw.clone());
            match s.media.get(&format!("{server}/{id}")) {
                Some(b) => Response::json(json!(null)).with_body(b),
                None => error(404, "M_NOT_FOUND", "no such media"),
            }
        }
        _ => error(404, "M_UNRECOGNIZED", "Unrecognized request"),
    }
}

trait WithBody {
    fn with_body(self, b: &str) -> Self;
}

impl WithBody for Response {
    fn with_body(mut self, b: &str) -> Self {
        self.body = b.to_string();
        self
    }
}
