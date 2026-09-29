//! M39: a mock signal-cli HTTP daemon: `/api/v1/rpc` (JSON-RPC) and the
//! `/api/v1/events` stream, kept open and fed by `push`. What is pushed
//! while nobody listens waits for the next listener, as signal-cli's
//! `--receive-mode on-connection` leaves it on Signal's servers.

use super::http::read;
use base64::Engine;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::broadcast;

pub const ACCOUNT: &str = "+15550000001";
pub const BOT_UUID: &str = "0b0b0b0b-0000-4000-8000-000000000001";
pub const MAX: &str = "+15550000002";
pub const MAX_UUID: &str = "0a0a0a0a-0000-4000-8000-000000000002";
pub const STRANGER: &str = "+15550000009";
/// A number that isn't on Signal.
pub const NOBODY: &str = "+15550000404";
pub const GROUP: &str = "R3JvdXAtb25lLWJhc2U2NC1pZC0wMDAwMDAwMDA9";
pub const OTHER_GROUP: &str = "T3RoZXItZ3JvdXAtYmFzZTY0LWlkLTAwMDAwMDA9";

#[derive(Default)]
pub struct State {
    /// (method, params) of every call.
    pub calls: Vec<(String, Value)>,
    /// (code, message) the next call of a method answers with.
    pub fail_next: HashMap<String, VecDeque<(i64, String)>>,
    /// The daemon serves several accounts: calls must name one.
    pub multi: bool,
    /// Event streams opened, and open now.
    pub streams: usize,
    pub open: usize,
    /// Events pushed while nobody listened.
    pub queued: VecDeque<String>,
    /// Attachment id → bytes.
    pub attachments: HashMap<String, Vec<u8>>,
    next_ts: i64,
    /// Bumped by `cut`: a stream of an older generation is gone.
    gen: u64,
}

pub struct SignalDaemon {
    pub url: String,
    pub port: u16,
    pub state: Arc<Mutex<State>>,
    events: broadcast::Sender<Option<String>>,
}

impl SignalDaemon {
    pub fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// Sends one event (`{envelope, account}`) to whoever listens.
    pub fn push(&self, v: Value) {
        let data = v.to_string();
        let mut st = self.state();
        if st.open == 0 || self.events.send(Some(data.clone())).is_err() {
            st.queued.push_back(data);
        }
    }

    /// Ends every open event stream.
    pub fn cut(&self) {
        let mut st = self.state();
        st.gen += 1;
        st.open = 0;
        let _ = self.events.send(None);
    }

    pub fn fail_next(&self, method: &str, code: i64, message: &str) {
        self.state()
            .fail_next
            .entry(method.into())
            .or_default()
            .push_back((code, message.into()));
    }

    /// Params of every call of `method`.
    pub fn calls(&self, method: &str) -> Vec<Value> {
        self.state()
            .calls
            .iter()
            .filter(|(m, _)| m == method)
            .map(|(_, p)| p.clone())
            .collect()
    }
}

/// A DM from `from` (a number, with Max's uuid when it is Max).
pub fn dm(from: &str, text: &str, ts: i64) -> Value {
    let uuid = if from == MAX {
        MAX_UUID
    } else {
        "0c0c0c0c-0000-4000-8000-000000000009"
    };
    json!({
        "account": ACCOUNT,
        "envelope": {
            "source": from,
            "sourceNumber": from,
            "sourceUuid": uuid,
            "sourceName": if from == MAX { "Max" } else { "Eve" },
            "sourceDevice": 1,
            "timestamp": ts,
            "dataMessage": { "timestamp": ts, "message": text, "expiresInSeconds": 0 }
        }
    })
}

/// A message in `group`; `mention_us` puts our mention first.
pub fn group(from: &str, group: &str, text: &str, ts: i64, mention_us: bool) -> Value {
    let mut v = dm(from, text, ts);
    let d = &mut v["envelope"]["dataMessage"];
    d["groupInfo"] = json!({ "groupId": group, "type": "DELIVER" });
    if mention_us {
        d["message"] = json!(format!("\u{FFFC} {text}"));
        d["mentions"] = json!([{ "name": ACCOUNT, "number": ACCOUNT, "uuid": BOT_UUID, "start": 0, "length": 1 }]);
    }
    v
}

pub fn start() -> SignalDaemon {
    let (listener, port) = super::bind();
    start_on(listener, port)
}

pub fn start_on(listener: std::net::TcpListener, port: u16) -> SignalDaemon {
    let state = Arc::new(Mutex::new(State {
        next_ts: 1_760_000_100_000,
        ..State::default()
    }));
    let (events, _) = broadcast::channel(64);
    let d = SignalDaemon {
        url: format!("http://127.0.0.1:{port}"),
        port,
        state: state.clone(),
        events: events.clone(),
    };
    super::runtime().spawn(async move {
        let listener = TcpListener::from_std(listener).unwrap();
        while let Ok((mut stream, _)) = listener.accept().await {
            let state = state.clone();
            let events = events.clone();
            tokio::spawn(async move {
                let Some(req) = read(&mut stream).await else {
                    return;
                };
                if req.method == "GET" && req.path.starts_with("/api/v1/events") {
                    let multi = state.lock().unwrap().multi;
                    if multi && !req.path.contains("account=%2B") {
                        let _ = stream
                            .write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                            .await;
                        return;
                    }
                    let mut rx = events.subscribe();
                    let (gen, queued): (u64, Vec<String>) = {
                        let mut st = state.lock().unwrap();
                        st.streams += 1;
                        st.open += 1;
                        (st.gen, st.queued.drain(..).collect())
                    };
                    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n:\n\n";
                    let mut ok = stream.write_all(head.as_bytes()).await.is_ok();
                    for data in queued {
                        ok = ok && write_event(&mut stream, &data).await;
                    }
                    while ok {
                        match rx.recv().await {
                            Ok(Some(data)) => ok = write_event(&mut stream, &data).await,
                            Ok(None) | Err(_) => break,
                        }
                    }
                    {
                        let mut st = state.lock().unwrap();
                        if st.gen == gen {
                            st.open -= 1;
                        }
                    }
                    let _ = stream.shutdown().await;
                    return;
                }
                let (status, body) = if req.method == "POST" && req.path == "/api/v1/rpc" {
                    (200, rpc(&state, req.json()).to_string())
                } else {
                    (404, String::new())
                };
                let head = format!(
                    "HTTP/1.1 {status} X\r\ncontent-length: {}\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(body.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    d
}

async fn write_event(stream: &mut tokio::net::TcpStream, data: &str) -> bool {
    let frame = format!("event:receive\ndata:{data}\n\n");
    stream.write_all(frame.as_bytes()).await.is_ok() && stream.flush().await.is_ok()
}

fn rpc(state: &Mutex<State>, req: Value) -> Value {
    let id = req["id"].clone();
    let method = req["method"].as_str().unwrap_or("").to_string();
    let params = req["params"].clone();
    let mut st = state.lock().unwrap();
    st.calls.push((method.clone(), params.clone()));
    let err = |code: i64, message: &str| json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } });
    if st.multi && params["account"].as_str() != Some(ACCOUNT) {
        return err(-32602, "Method requires valid account parameter");
    }
    if let Some((code, message)) = st.fail_next.get_mut(&method).and_then(VecDeque::pop_front) {
        return err(code, &message);
    }
    st.next_ts += 1;
    let ts = st.next_ts;
    let result = match method.as_str() {
        "version" => json!({ "version": "0.13.9" }),
        "getUserStatus" => {
            json!([{ "recipient": ACCOUNT, "number": ACCOUNT, "uuid": BOT_UUID, "isRegistered": true }])
        }
        "listGroups" => json!([
            { "id": GROUP, "name": "Team", "isMember": true },
            { "id": OTHER_GROUP, "name": "Family", "isMember": true },
            { "id": "TGVmdC1ncm91cA==", "name": "Left", "isMember": false }
        ]),
        "send" => {
            let to = params["recipient"][0].as_str().unwrap_or("");
            let kind = if to == NOBODY {
                "UNREGISTERED_FAILURE"
            } else {
                "SUCCESS"
            };
            json!({ "timestamp": ts, "results": [{ "recipientAddress": { "number": to }, "type": kind }] })
        }
        "sendReaction" | "sendReceipt" => json!({ "timestamp": ts }),
        "getAttachment" => {
            let aid = params["id"].as_str().unwrap_or("");
            match st.attachments.get(aid) {
                Some(b) => json!({ "data": base64::engine::general_purpose::STANDARD.encode(b) }),
                None => return err(-1, "Attachment file not found"),
            }
        }
        _ => return err(-32601, "Method not implemented"),
    };
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}
