//! A mock Discord: the REST API under `/api/v10` and a Gateway WebSocket
//! that says HELLO, answers IDENTIFY with READY and RESUME with RESUMED,
//! ACKs heartbeats, and does whatever a test plans for the next handshake
//! (close with a code, invalidate the session) or tells it now (dispatch an
//! event, ask for a reconnect).

use super::http::{self, Request, Response};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

pub const TOKEN: &str = "MTAxMDEwMTAxMDEw.GmOck1.mock-discord-token-do-not-log-0123456";
pub const BOT: &str = "999";
pub const APP: &str = "555";

/// What the next IDENTIFY or RESUME gets.
#[derive(Debug, Clone)]
pub enum Plan {
    Ready,
    Close(u16),
    Invalid(bool),
}

pub enum Cmd {
    Dispatch(&'static str, Value),
    Op(Value),
    Close(u16),
}

pub struct State {
    pub requests: Vec<Request>,
    /// Every frame the client sent on any connection.
    pub frames: Vec<Value>,
    pub connections: usize,
    /// Served once each, first match: `("POST", "/channels/1/messages")`.
    pub overrides: Vec<(String, String, Response)>,
    /// `GET /channels/{id}`.
    pub channels: HashMap<String, Value>,
    pub plans: VecDeque<Plan>,
    pub ack: bool,
    pub heartbeat_ms: u64,
    /// Handshakes answered with READY / RESUMED.
    pub readies: usize,
    pub resumed: usize,
    next_id: u64,
    seq: u64,
    ws_url: String,
    token: String,
}

impl State {
    pub fn identifies(&self) -> Vec<&Value> {
        self.frames.iter().filter(|f| f["op"] == 2).collect()
    }

    pub fn resumes(&self) -> Vec<&Value> {
        self.frames.iter().filter(|f| f["op"] == 6).collect()
    }

    /// `(channel, body)` of every message created.
    pub fn messages(&self) -> Vec<(String, Value)> {
        self.requests
            .iter()
            .filter(|r| r.method == "POST" && r.path.ends_with("/messages"))
            .filter_map(|r| {
                let c = r
                    .path
                    .strip_prefix("/api/v10/channels/")?
                    .split('/')
                    .next()?;
                Some((c.to_string(), r.json()))
            })
            .collect()
    }

    pub fn find(&self, method: &str, path_part: &str) -> Vec<Request> {
        self.requests
            .iter()
            .filter(|r| r.method == method && r.path.contains(path_part))
            .cloned()
            .collect()
    }
}

pub struct Discord {
    pub api: String,
    pub ws_url: String,
    state: Arc<Mutex<State>>,
    cmd: mpsc::UnboundedSender<Cmd>,
}

impl Discord {
    pub fn start() -> Self {
        Self::start_with_token(TOKEN)
    }

    pub fn start_with_token(token: &str) -> Self {
        let (ws_listener, ws_port) = super::bind();
        let ws_url = format!("ws://127.0.0.1:{ws_port}");
        let state = Arc::new(Mutex::new(State {
            requests: vec![],
            frames: vec![],
            connections: 0,
            overrides: vec![],
            channels: HashMap::new(),
            plans: VecDeque::new(),
            ack: true,
            heartbeat_ms: 45_000,
            readies: 0,
            resumed: 0,
            next_id: 5000,
            seq: 0,
            ws_url: ws_url.clone(),
            token: token.to_string(),
        }));
        let s = state.clone();
        let port = http::serve(Arc::new(move |req| rest(&s, req)));
        let (cmd, rx) = mpsc::unbounded_channel();
        let rx = Arc::new(tokio::sync::Mutex::new(rx));
        let s = state.clone();
        super::runtime().spawn(async move {
            let listener = tokio::net::TcpListener::from_std(ws_listener).unwrap();
            while let Ok((stream, _)) = listener.accept().await {
                let (s, rx) = (s.clone(), rx.clone());
                tokio::spawn(async move {
                    if let Ok(ws) = tokio_tungstenite::accept_async(stream).await {
                        connection(ws, s, rx).await;
                    }
                });
            }
        });
        Self {
            api: format!("http://127.0.0.1:{port}/api/v10"),
            ws_url,
            state,
            cmd,
        }
    }

    pub fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    pub fn dispatch(&self, t: &'static str, d: Value) {
        self.cmd.send(Cmd::Dispatch(t, d)).unwrap();
    }

    pub fn op(&self, v: Value) {
        self.cmd.send(Cmd::Op(v)).unwrap();
    }

    pub fn close(&self, code: u16) {
        self.cmd.send(Cmd::Close(code)).unwrap();
    }

    /// Answers the next request matching once with `resp`.
    pub fn once(&self, method: &str, path_part: &str, resp: Response) {
        self.state()
            .overrides
            .push((method.into(), path_part.into(), resp));
    }

    /// A DM from `user` (its DM channel is `7<user>`).
    pub fn dm(&self, id: &str, user: &str, content: &str) {
        self.dispatch("MESSAGE_CREATE", dm(id, user, content));
    }
}

/// A DM's MESSAGE_CREATE.
pub fn dm(id: &str, user: &str, content: &str) -> Value {
    json!({
        "id": id, "channel_id": format!("7{user}"), "content": content,
        "author": { "id": user, "username": format!("user{user}") },
        "timestamp": "2026-09-26T10:00:00+00:00", "mentions": [], "attachments": [],
    })
}

/// A guild MESSAGE_CREATE in `channel`, mentioning the bot when `mention`.
pub fn guild(id: &str, channel: &str, user: &str, content: &str, mention: bool) -> Value {
    let mentions = if mention {
        json!([{ "id": BOT, "username": "ferrule" }])
    } else {
        json!([])
    };
    json!({
        "id": id, "channel_id": channel, "guild_id": "42", "content": content,
        "author": { "id": user, "username": format!("user{user}") },
        "timestamp": "2026-09-26T10:00:00+00:00", "mentions": mentions, "attachments": [],
    })
}

fn rest(state: &Mutex<State>, req: Request) -> Response {
    let mut s = state.lock().unwrap();
    s.requests.push(req.clone());
    if let Some(i) = s
        .overrides
        .iter()
        .position(|(m, p, _)| *m == req.method && req.path.contains(p.as_str()))
    {
        return s.overrides.remove(i).2;
    }
    let path = req
        .path
        .strip_prefix("/api/v10")
        .unwrap_or(&req.path)
        .to_string();
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let own_auth = matches!(parts[0], "interactions" | "webhooks");
    if !own_auth && req.header("authorization") != Some(&format!("Bot {}", s.token)) {
        return Response::json(json!({ "message": "401: Unauthorized", "code": 0 }))
            .with_status(401);
    }
    let id = |s: &mut State| {
        s.next_id += 1;
        s.next_id.to_string()
    };
    match (req.method.as_str(), parts.as_slice()) {
        ("GET", ["gateway", "bot"]) => Response::json(json!({
            "url": s.ws_url, "shards": 1,
            "session_start_limit": { "total": 1000, "remaining": 990, "reset_after": 1000, "max_concurrency": 1 },
        })),
        ("GET", ["users", "@me"]) => {
            Response::json(json!({ "id": BOT, "username": "ferrule-bot", "bot": true }))
        }
        ("GET", ["applications", "@me"]) => {
            Response::json(json!({ "id": APP, "name": "ferrule", "flags": 1 << 19 }))
        }
        ("POST", ["users", "@me", "channels"]) => {
            let r = req.json()["recipient_id"]
                .as_str()
                .unwrap_or("")
                .to_string();
            Response::json(json!({ "id": format!("7{r}"), "type": 1 }))
        }
        ("POST", ["channels", c, "messages"]) => {
            let c = c.to_string();
            let id = id(&mut s);
            Response::json(json!({ "id": id, "channel_id": c }))
        }
        ("PATCH", ["channels", _, "messages", m]) => Response::json(json!({ "id": m })),
        ("PUT", ["channels", _, "messages", _, "reactions", _, "@me"]) => Response::empty(204),
        ("GET", ["channels", c]) => match s.channels.get(*c) {
            Some(v) => Response::json(v.clone()),
            None => Response::json(json!({ "message": "Unknown Channel", "code": 10003 }))
                .with_status(404),
        },
        ("POST", ["interactions", _, _, "callback"]) => Response::empty(204),
        ("PATCH", ["webhooks", _, _, "messages", "@original"]) => {
            let id = id(&mut s);
            Response::json(json!({ "id": id }))
        }
        ("PUT", ["applications", _, "commands"]) => Response::json(req.json()),
        _ => Response::json(json!({ "message": "404: Not Found", "code": 0 })).with_status(404),
    }
}

type Ws = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

async fn send(ws: &mut Ws, v: Value) -> bool {
    ws.send(Message::Text(v.to_string().into())).await.is_ok()
}

async fn close(ws: &mut Ws, code: u16) {
    let _ = ws
        .send(Message::Close(Some(CloseFrame {
            code: CloseCode::from(code),
            reason: "mock".into(),
        })))
        .await;
    // Let the close reach the client before the socket is dropped.
    let _ = tokio::time::timeout(std::time::Duration::from_millis(500), async {
        while let Some(Ok(_)) = ws.next().await {}
    })
    .await;
}

async fn connection(
    mut ws: Ws,
    state: Arc<Mutex<State>>,
    rx: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<Cmd>>>,
) {
    let interval = {
        let mut s = state.lock().unwrap();
        s.connections += 1;
        s.heartbeat_ms
    };
    if !send(
        &mut ws,
        json!({ "op": 10, "d": { "heartbeat_interval": interval } }),
    )
    .await
    {
        return;
    }
    let mut rx = rx.lock().await;
    loop {
        tokio::select! {
            frame = ws.next() => {
                let text = match frame {
                    Some(Ok(Message::Text(t))) => t.to_string(),
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                    Some(Ok(_)) => continue,
                };
                let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                let (reply, closing) = {
                    let mut s = state.lock().unwrap();
                    s.frames.push(v.clone());
                    match v["op"].as_u64() {
                        Some(1) => (s.ack.then(|| json!({ "op": 11 })), None),
                        Some(op @ (2 | 6)) => match s.plans.pop_front().unwrap_or(Plan::Ready) {
                            Plan::Close(code) => (None, Some(code)),
                            Plan::Invalid(d) => (Some(json!({ "op": 9, "d": d })), None),
                            Plan::Ready if op == 2 => {
                                s.readies += 1;
                                s.seq += 1;
                                let url = s.ws_url.clone();
                                (Some(json!({ "op": 0, "t": "READY", "s": s.seq, "d": {
                                    "v": 10, "session_id": format!("sess{}", s.readies),
                                    "resume_gateway_url": url,
                                    "user": { "id": BOT, "username": "ferrule-bot", "bot": true },
                                    "application": { "id": APP, "flags": 1 << 19 },
                                    "guilds": [],
                                }})), None)
                            }
                            Plan::Ready => {
                                s.resumed += 1;
                                s.seq += 1;
                                (Some(json!({ "op": 0, "t": "RESUMED", "s": s.seq, "d": {} })), None)
                            }
                        },
                        _ => (None, None),
                    }
                };
                if let Some(code) = closing {
                    close(&mut ws, code).await;
                    return;
                }
                if let Some(r) = reply {
                    if !send(&mut ws, r).await {
                        return;
                    }
                }
            }
            cmd = rx.recv() => {
                let out = match cmd {
                    Some(Cmd::Dispatch(t, d)) => {
                        let mut s = state.lock().unwrap();
                        s.seq += 1;
                        json!({ "op": 0, "t": t, "s": s.seq, "d": d })
                    }
                    Some(Cmd::Op(v)) => v,
                    Some(Cmd::Close(code)) => {
                        close(&mut ws, code).await;
                        return;
                    }
                    None => return,
                };
                if !send(&mut ws, out).await {
                    return;
                }
            }
        }
    }
}
