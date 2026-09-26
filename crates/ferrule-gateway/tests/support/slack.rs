//! A mock Slack: the Web API under `/api/<method>` and a Socket Mode
//! WebSocket that says `hello`, records every frame the client sends (the
//! acks), and sends whatever a test tells it now (an envelope, a
//! `disconnect`, a close).

use super::http::{self, Request, Response};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

pub const BOT_TOKEN: &str = "xoxb-1111-2222-mockslackbottokendonotlog";
pub const APP_TOKEN: &str = "xapp-1-A111-3333-mockslackapptokendonotlog";
/// The bot's user id.
pub const BOT: &str = "UBOT";
/// The ticket in the socket's address: secret-ish, never logged.
pub const TICKET: &str = "mock-ticket-0f0f0f";

pub enum Cmd {
    Send(Value),
    Close,
}

pub struct State {
    pub requests: Vec<Request>,
    /// When each request came, alongside `requests`.
    pub times: Vec<Instant>,
    /// Every frame the client sent on any connection.
    pub frames: Vec<Value>,
    pub connections: usize,
    /// `apps.connections.open` calls answered.
    pub opens: usize,
    /// Served once each, first match by method name.
    pub overrides: Vec<(String, Response)>,
    next: u64,
    ws_url: String,
}

impl State {
    /// The requests to a Web API method, in order.
    pub fn calls(&self, method: &str) -> Vec<Request> {
        let path = format!("/api/{method}");
        self.requests
            .iter()
            .filter(|r| r.path == path)
            .cloned()
            .collect()
    }

    /// When each call to `method` came.
    pub fn times_of(&self, method: &str) -> Vec<Instant> {
        let path = format!("/api/{method}");
        self.requests
            .iter()
            .zip(&self.times)
            .filter(|(r, _)| r.path == path)
            .map(|(_, t)| *t)
            .collect()
    }

    /// The `chat.postMessage` bodies.
    pub fn posts(&self) -> Vec<Value> {
        self.calls("chat.postMessage")
            .iter()
            .map(Request::json)
            .collect()
    }

    /// The envelope ids acknowledged.
    pub fn acks(&self) -> Vec<String> {
        self.frames
            .iter()
            .filter_map(|f| f["envelope_id"].as_str().map(str::to_string))
            .collect()
    }
}

pub struct Slack {
    pub api: String,
    state: Arc<Mutex<State>>,
    cmd: mpsc::UnboundedSender<Cmd>,
    envelopes: std::sync::atomic::AtomicU64,
}

impl Slack {
    pub fn start() -> Self {
        let (ws_listener, ws_port) = super::bind();
        let ws_url = format!("ws://127.0.0.1:{ws_port}/link/?ticket={TICKET}");
        let state = Arc::new(Mutex::new(State {
            requests: vec![],
            times: vec![],
            frames: vec![],
            connections: 0,
            opens: 0,
            overrides: vec![],
            next: 0,
            ws_url,
        }));
        let s = state.clone();
        let port = http::serve(Arc::new(move |req| api(&s, req)));
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
            api: format!("http://127.0.0.1:{port}/api"),
            state,
            cmd,
            envelopes: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// Sends a frame on the open socket.
    pub fn send(&self, v: Value) {
        self.cmd.send(Cmd::Send(v)).unwrap();
    }

    pub fn close(&self) {
        self.cmd.send(Cmd::Close).unwrap();
    }

    /// Answers the next call to `method` with `resp`.
    pub fn once(&self, method: &str, resp: Response) {
        self.state().overrides.push((method.into(), resp));
    }

    /// Sends an envelope of `kind` with `payload`; its `envelope_id`.
    pub fn envelope(&self, kind: &str, payload: Value) -> String {
        let n = self
            .envelopes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let id = format!("env-{n}");
        self.send(json!({
            "envelope_id": id, "type": kind, "payload": payload,
            "accepts_response_payload": false, "retry_attempt": 0,
        }));
        id
    }

    /// An `events_api` envelope for `event`, with `event_id`.
    pub fn event(&self, event_id: &str, event: Value) -> String {
        self.envelope(
            "events_api",
            json!({ "type": "event_callback", "event_id": event_id, "event": event }),
        )
    }
}

/// A DM from `user` (its DM channel is `D<user>`).
pub fn dm(user: &str, ts: &str, text: &str) -> Value {
    json!({
        "type": "message", "channel_type": "im", "channel": format!("D{user}"),
        "user": user, "text": text, "ts": ts,
        "user_profile": { "display_name": format!("name-{user}") },
    })
}

/// An `@mention` of the bot in `channel`.
pub fn mention(channel: &str, user: &str, ts: &str, text: &str) -> Value {
    json!({
        "type": "app_mention", "channel": channel, "user": user,
        "text": format!("<@{BOT}> {text}"), "ts": ts,
    })
}

fn api(state: &Mutex<State>, req: Request) -> Response {
    let mut s = state.lock().unwrap();
    s.requests.push(req.clone());
    s.times.push(Instant::now());
    let method = req.path.strip_prefix("/api/").unwrap_or("").to_string();
    if let Some(i) = s.overrides.iter().position(|(m, _)| *m == method) {
        return s.overrides.remove(i).1;
    }
    let want = if method == "apps.connections.open" {
        APP_TOKEN
    } else {
        BOT_TOKEN
    };
    if req.header("authorization") != Some(&format!("Bearer {want}")) {
        return Response::json(json!({ "ok": false, "error": "invalid_auth" }));
    }
    let body = req.json();
    match method.as_str() {
        "auth.test" => Response::json(json!({
            "ok": true, "user_id": BOT, "user": "ferrule", "team": "Mock Team", "bot_id": "B1",
        })),
        "apps.connections.open" => {
            s.opens += 1;
            Response::json(json!({ "ok": true, "url": s.ws_url }))
        }
        "conversations.open" => {
            let u = body["users"].as_str().unwrap_or("").to_string();
            Response::json(json!({ "ok": true, "channel": { "id": format!("D{u}") } }))
        }
        "chat.postMessage" => {
            s.next += 1;
            let ts = format!("1790000000.{:06}", s.next);
            Response::json(json!({ "ok": true, "channel": body["channel"], "ts": ts }))
        }
        "chat.update" => {
            Response::json(json!({ "ok": true, "channel": body["channel"], "ts": body["ts"] }))
        }
        "reactions.add" => Response::json(json!({ "ok": true })),
        _ => Response::json(json!({ "ok": false, "error": "unknown_method" })),
    }
}

type Ws = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

async fn connection(
    mut ws: Ws,
    state: Arc<Mutex<State>>,
    rx: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<Cmd>>>,
) {
    state.lock().unwrap().connections += 1;
    let hello =
        json!({ "type": "hello", "num_connections": 1, "connection_info": { "app_id": "A111" } });
    if ws
        .send(Message::Text(hello.to_string().into()))
        .await
        .is_err()
    {
        return;
    }
    let mut rx = rx.lock().await;
    loop {
        tokio::select! {
            frame = ws.next() => match frame {
                Some(Ok(Message::Text(t))) => {
                    let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                    state.lock().unwrap().frames.push(v);
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(_)) => {}
            },
            cmd = rx.recv() => match cmd {
                Some(Cmd::Send(v)) => {
                    let disconnect = v["type"] == "disconnect";
                    if ws.send(Message::Text(v.to_string().into())).await.is_err() {
                        return;
                    }
                    if disconnect {
                        // The client closes; this socket takes no more commands.
                        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                            while let Some(Ok(_)) = ws.next().await {}
                        })
                        .await;
                        return;
                    }
                }
                Some(Cmd::Close) => {
                    let _ = ws.close(None).await;
                    return;
                }
                None => return,
            },
        }
    }
}
