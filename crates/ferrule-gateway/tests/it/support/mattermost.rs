//! M39: a mock Mattermost server on one port: REST under `/api/v4` and
//! the WebSocket at `/api/v4/websocket`, which checks the
//! `authentication_challenge`, records every frame the client sends (the
//! challenge, the pings), and sends whatever a test tells it now.

use super::http::{read, Request, Response};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

pub const TOKEN: &str = "mmbottoken0mockdonotlog00a";
/// The bot's user id, and the people's.
pub const BOT: &str = "b0000000000000000000000bot";
pub const MAX: &str = "m0000000000000000000000max";
pub const STRANGER: &str = "e0000000000000000000000eve";
/// The DM channel between the bot and Max.
pub const DM: &str = "d00000000000000000000dmmax";
/// An allowed channel, and one that isn't.
pub const CHANNEL: &str = "c000000000000000000000town";
pub const OTHER: &str = "c0000000000000000000000ops";
pub const TEAM: &str = "t000000000000000000000acme";

pub enum Cmd {
    Send(Value),
    Close,
}

#[derive(Default)]
pub struct State {
    pub requests: Vec<Request>,
    /// Every frame the client sent on any connection.
    pub frames: Vec<Value>,
    pub connections: usize,
    /// Served once each, first match by "METHOD /path" prefix.
    pub overrides: Vec<(String, Response)>,
    /// File id → (name, mime, text) for downloads.
    pub files: HashMap<String, (String, String, String)>,
    /// Refuse every REST call with a 401.
    pub revoked: bool,
    next: u64,
}

impl State {
    /// Requests whose "METHOD /path" starts with `prefix`.
    pub fn calls(&self, prefix: &str) -> Vec<Request> {
        self.requests
            .iter()
            .filter(|r| format!("{} {}", r.method, r.path).starts_with(prefix))
            .cloned()
            .collect()
    }

    /// The `POST /posts` bodies.
    pub fn posts(&self) -> Vec<Value> {
        self.calls("POST /api/v4/posts")
            .iter()
            .map(Request::json)
            .collect()
    }

    /// The posts in `channel`.
    pub fn posts_in(&self, channel: &str) -> Vec<Value> {
        self.posts()
            .into_iter()
            .filter(|p| p["channel_id"] == channel)
            .collect()
    }

    /// `(post, emoji)` of every reaction the bot added.
    pub fn reactions(&self) -> Vec<(String, String)> {
        self.calls("POST /api/v4/reactions")
            .iter()
            .map(Request::json)
            .map(|v| {
                (
                    v["post_id"].as_str().unwrap_or("").to_string(),
                    v["emoji_name"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect()
    }
}

pub struct Mattermost {
    pub url: String,
    state: Arc<Mutex<State>>,
    cmd: mpsc::UnboundedSender<Cmd>,
}

impl Mattermost {
    pub fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// Sends an event on the open socket.
    pub fn send(&self, v: Value) {
        self.cmd.send(Cmd::Send(v)).unwrap();
    }

    pub fn close(&self) {
        self.cmd.send(Cmd::Close).unwrap();
    }

    /// Answers the next request matching "METHOD /path" prefix with `resp`.
    pub fn once(&self, prefix: &str, resp: Response) {
        self.state().overrides.push((prefix.into(), resp));
    }

    /// A file people can send.
    pub fn file(&self, id: &str, name: &str, mime: &str, text: &str) {
        self.state()
            .files
            .insert(id.into(), (name.into(), mime.into(), text.into()));
    }
}

/// A 26-character id for the `n`th thing the mock made.
fn id(prefix: char, n: u64) -> String {
    format!("{prefix}{n:025}")
}

/// A `posted` event.
pub fn posted(channel: &str, channel_type: &str, post: Value, mentions: &[&str]) -> Value {
    let user = post["user_id"].as_str().unwrap_or("").to_string();
    let sender = match user.as_str() {
        MAX => "@max",
        STRANGER => "@eve",
        BOT => "@ferrule",
        _ => "@someone",
    };
    let mut data = json!({
        "channel_type": channel_type,
        "channel_display_name": "x",
        "sender_name": sender,
        "team_id": TEAM,
        "post": post.to_string(),
    });
    if !mentions.is_empty() {
        data["mentions"] = json!(serde_json::to_string(mentions).unwrap());
    }
    json!({ "event": "posted", "data": data, "broadcast": { "channel_id": channel }, "seq": 5 })
}

/// A post's JSON.
pub fn post(id: &str, channel: &str, user: &str, text: &str, root: Option<&str>) -> Value {
    json!({
        "id": id, "channel_id": channel, "user_id": user, "message": text,
        "root_id": root.unwrap_or(""), "type": "", "props": {},
        "create_at": 1_760_000_000_000_i64, "update_at": 1_760_000_000_000_i64,
    })
}

/// A DM from `user` (Max's DM channel is `DM`).
pub fn dm(id: &str, user: &str, text: &str) -> Value {
    let channel = if user == MAX {
        DM.to_string()
    } else {
        format!("d{}", &user[1..])
    };
    posted(&channel, "D", post(id, &channel, user, text, None), &[])
}

/// A message in `channel`, `@ferrule` mentioned when `mention`.
pub fn said(
    id: &str,
    channel: &str,
    user: &str,
    text: &str,
    root: Option<&str>,
    mention: bool,
) -> Value {
    let (text, mentions): (String, &[&str]) = if mention {
        (format!("@ferrule {text}"), &[BOT])
    } else {
        (text.to_string(), &[])
    };
    posted(channel, "O", post(id, channel, user, &text, root), mentions)
}

/// `reaction_added`: `user` reacted `emoji` on `post`.
pub fn reaction(user: &str, post: &str, emoji: &str) -> Value {
    let r = json!({ "user_id": user, "post_id": post, "emoji_name": emoji, "create_at": 1_760_000_001_000_i64 });
    json!({ "event": "reaction_added", "data": { "reaction": r.to_string() }, "broadcast": { "channel_id": DM }, "seq": 6 })
}

pub fn start() -> Mattermost {
    let (listener, port) = super::bind();
    let state = Arc::new(Mutex::new(State::default()));
    let (cmd, rx) = mpsc::unbounded_channel();
    let rx = Arc::new(tokio::sync::Mutex::new(rx));
    let s = state.clone();
    super::runtime().spawn(async move {
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        while let Ok((mut stream, _)) = listener.accept().await {
            let (s, rx) = (s.clone(), rx.clone());
            tokio::spawn(async move {
                if is_upgrade(&stream).await {
                    if let Ok(ws) = tokio_tungstenite::accept_async(stream).await {
                        connection(ws, s, rx).await;
                    }
                    return;
                }
                let Some(req) = read(&mut stream).await else {
                    return;
                };
                let resp = api(&s, req);
                let mut head = format!(
                    "HTTP/1.1 {} X\r\ncontent-length: {}\r\nconnection: close\r\ncontent-type: application/json\r\n",
                    resp.status,
                    resp.body.len()
                );
                for (k, v) in &resp.headers {
                    head.push_str(&format!("{k}: {v}\r\n"));
                }
                head.push_str("\r\n");
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(resp.body.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    Mattermost {
        url: format!("http://127.0.0.1:{port}"),
        state,
        cmd,
    }
}

/// Whether the request is the WebSocket's (peeked, not read).
async fn is_upgrade(stream: &tokio::net::TcpStream) -> bool {
    let want = b"GET /api/v4/websocket";
    let mut buf = [0u8; 32];
    for _ in 0..200 {
        match stream.peek(&mut buf).await {
            Ok(n) if n >= want.len() => return &buf[..want.len()] == want,
            Ok(0) | Err(_) => return false,
            Ok(_) => tokio::time::sleep(std::time::Duration::from_millis(5)).await,
        }
    }
    false
}

fn err(status: u16, id: &str, message: &str) -> Response {
    Response::json(json!({ "id": id, "message": message, "status_code": status }))
        .with_status(status)
}

fn api(state: &Mutex<State>, req: Request) -> Response {
    let mut s = state.lock().unwrap();
    s.requests.push(req.clone());
    let line = format!("{} {}", req.method, req.path);
    if let Some(i) = s
        .overrides
        .iter()
        .position(|(p, _)| line.starts_with(p.as_str()))
    {
        return s.overrides.remove(i).1;
    }
    if s.revoked || req.header("authorization") != Some(&format!("Bearer {TOKEN}")) {
        return err(
            401,
            "api.context.session_expired.app_error",
            "Invalid or expired session, please login again.",
        );
    }
    let Some(path) = req.path.strip_prefix("/api/v4") else {
        return Response::empty(404);
    };
    let body = req.json();
    match (req.method.as_str(), path) {
        ("GET", "/users/me") => {
            Response::json(json!({ "id": BOT, "username": "ferrule", "is_bot": true }))
        }
        ("POST", "/users/usernames") => {
            let found: Vec<Value> = body
                .as_array()
                .into_iter()
                .flatten()
                .filter(|n| *n == "max")
                .map(|_| json!({ "id": MAX, "username": "max" }))
                .collect();
            Response::json(json!(found))
        }
        ("GET", p) if p.starts_with("/users/username/") => match &p[16..] {
            "max" => Response::json(json!({ "id": MAX, "username": "max" })),
            _ => err(
                404,
                "app.user.missing_account.const",
                "Unable to find the user.",
            ),
        },
        ("GET", "/users/me/teams") => {
            Response::json(json!([{ "id": TEAM, "display_name": "Acme" }]))
        }
        ("GET", p) if p.starts_with("/users/me/teams/") && p.ends_with("/channels") => {
            Response::json(json!([
                { "id": CHANNEL, "type": "O", "display_name": "Town Square", "name": "town-square" },
                { "id": OTHER, "type": "P", "display_name": "", "name": "ops" },
                { "id": DM, "type": "D", "display_name": "", "name": "x__y" }
            ]))
        }
        ("POST", "/channels/direct") => {
            let other = body
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .find(|u| *u != BOT)
                .unwrap_or("")
                .to_string();
            let ch = if other == MAX {
                DM.to_string()
            } else {
                format!("d{}", other.get(1..).unwrap_or(""))
            };
            Response::json(json!({ "id": ch, "type": "D" }))
        }
        ("POST", "/channels/members/me/view") => Response::json(json!({ "status": "OK" })),
        ("POST", "/posts") => {
            s.next += 1;
            let mut p = body.clone();
            p["id"] = json!(id('p', s.next));
            p["user_id"] = json!(BOT);
            Response::json(p).with_status(201)
        }
        ("PUT", p) if p.starts_with("/posts/") && p.ends_with("/patch") => {
            let pid = &p[7..p.len() - 6];
            Response::json(json!({ "id": pid, "message": body["message"] }))
        }
        ("POST", "/reactions") => Response::json(body).with_status(201),
        ("POST", "/files") => {
            s.next += 1;
            Response::json(json!({ "file_infos": [{ "id": id('f', s.next) }] })).with_status(201)
        }
        ("GET", p) if p.starts_with("/files/") => {
            let rest = &p[7..];
            let (fid, info) = match rest.strip_suffix("/info") {
                Some(f) => (f, true),
                None => (rest, false),
            };
            match s.files.get(fid) {
                Some((name, mime, text)) if info => Response::json(
                    json!({ "id": fid, "name": name, "mime_type": mime, "size": text.len() }),
                ),
                Some((_, _, text)) => Response {
                    status: 200,
                    headers: vec![],
                    body: text.clone(),
                },
                None => err(
                    404,
                    "app.file_info.get.app_error",
                    "Unable to get the file info.",
                ),
            }
        }
        _ => err(
            404,
            "api.context.404.app_error",
            "Sorry, we could not find the page.",
        ),
    }
}

type Ws = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

async fn connection(
    mut ws: Ws,
    state: Arc<Mutex<State>>,
    rx: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<Cmd>>>,
) {
    state.lock().unwrap().connections += 1;
    let mut rx = rx.lock().await;
    loop {
        tokio::select! {
            frame = ws.next() => match frame {
                Some(Ok(Message::Text(t))) => {
                    let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                    state.lock().unwrap().frames.push(v.clone());
                    let reply = match v["action"].as_str() {
                        Some("authentication_challenge") if v["data"]["token"] == TOKEN => {
                            json!({ "status": "OK", "seq_reply": v["seq"] })
                        }
                        Some("authentication_challenge") => json!({
                            "status": "FAIL", "seq_reply": v["seq"],
                            "error": { "id": "api.web_socket_router.not_authenticated.app_error", "message": "not authenticated" }
                        }),
                        Some("ping") => json!({ "status": "OK", "seq_reply": v["seq"], "data": { "text": "pong" } }),
                        _ => continue,
                    };
                    if ws.send(Message::Text(reply.to_string().into())).await.is_err() {
                        return;
                    }
                    if v["action"] == "authentication_challenge" && reply["status"] == "OK" {
                        let hello = json!({ "event": "hello", "data": { "server_version": "10.5.0" }, "seq": 0 });
                        let _ = ws.send(Message::Text(hello.to_string().into())).await;
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                Some(Ok(_)) => {}
            },
            cmd = rx.recv() => match cmd {
                Some(Cmd::Send(v)) => {
                    if ws.send(Message::Text(v.to_string().into())).await.is_err() {
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
