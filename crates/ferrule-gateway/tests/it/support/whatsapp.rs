//! M39: a mock of Meta's Graph API (messages, media upload and download)
//! and the relay Worker's WhatsApp mailbox, on one 127.0.0.1 port.

use super::http::{serve, Request, Response};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

pub const PHONE_ID: &str = "1110001";
pub const TOKEN: &str = "EAAtest-whatsapp-token";
pub const APP_SECRET: &str = "app-secret-123";
pub const VERIFY: &str = "verify-me";
pub const RELAY_KEY: &str = "relay-key-abc";
pub const MAX: &str = "972500000001";

#[derive(Default)]
pub struct State {
    /// Every `POST /<phone>/messages` body, in order.
    pub sent: Vec<Value>,
    /// Uploads: (type field, file bytes length, raw body contains name).
    pub uploads: Vec<(String, usize)>,
    /// Error codes the next sends answer with, one each.
    pub fail_next: VecDeque<i64>,
    /// The mailbox: configured (verify token, app secret), events.
    pub config: Option<(String, String)>,
    pub events: Vec<(u64, String, String)>,
    pub seq: u64,
    pub takes: usize,
    /// `after` of every take.
    pub afters: Vec<u64>,
    /// Requests without the right bearer.
    pub unauthorized: usize,
    next_id: u64,
}

pub struct Meta {
    pub url: String,
    pub state: Arc<Mutex<State>>,
}

impl Meta {
    pub fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// Leaves a webhook body in the mailbox, signed with `secret`.
    pub fn deliver(&self, body: &Value, secret: &str) {
        let raw = body.to_string();
        let sig = ferrule_gateway::channels::hmac::sign(secret.as_bytes(), raw.as_bytes());
        let mut s = self.state();
        s.seq += 1;
        let seq = s.seq;
        s.events.push((seq, raw, sig));
    }

    /// Texts sent to `chat` (text bodies, interactive bodies).
    pub fn texts(&self) -> Vec<String> {
        self.state()
            .sent
            .iter()
            .filter_map(|b| {
                b["text"]["body"]
                    .as_str()
                    .or_else(|| b["interactive"]["body"]["text"].as_str())
                    .map(str::to_string)
            })
            .collect()
    }

    pub fn of_type(&self, kind: &str) -> Vec<Value> {
        self.state()
            .sent
            .iter()
            .filter(|b| b["type"] == kind)
            .cloned()
            .collect()
    }
}

fn graph_error(code: i64) -> Response {
    let status = if code == 130429 { 429 } else { 400 };
    Response::json(json!({
        "error": {
            "message": format!("mock error {code}"),
            "type": "OAuthException",
            "code": code,
            "error_data": { "details": format!("details for {code}") },
        }
    }))
    .with_status(status)
}

/// Starts the mock.
pub fn start() -> Meta {
    let state = Arc::new(Mutex::new(State::default()));
    let st = state.clone();
    let port_cell: Arc<Mutex<u16>> = Arc::new(Mutex::new(0));
    let pc = port_cell.clone();
    let port = serve(Arc::new(move |req: Request| {
        handle(&st, *pc.lock().unwrap(), req)
    }));
    *port_cell.lock().unwrap() = port;
    Meta {
        url: format!("http://127.0.0.1:{port}"),
        state,
    }
}

fn handle(st: &Mutex<State>, port: u16, req: Request) -> Response {
    let path = req.path.split('?').next().unwrap_or("").to_string();
    // The relay mailbox.
    if let Some(rest) = path.strip_prefix("/wa/") {
        let bx = ferrule_gateway::channels::whatsapp::mailbox_box(RELAY_KEY);
        let Some(op) = rest.strip_prefix(&format!("{bx}/")) else {
            return Response::empty(404);
        };
        if req.header("authorization") != Some(&format!("Bearer {RELAY_KEY}")) {
            st.lock().unwrap().unauthorized += 1;
            return Response::empty(401);
        }
        let mut s = st.lock().unwrap();
        let v = req.json();
        return match op {
            "config" => {
                s.config = Some((
                    v["verify_token"].as_str().unwrap_or("").into(),
                    v["app_secret"].as_str().unwrap_or("").into(),
                ));
                Response::json(json!({"ok": true}))
            }
            "take" => {
                let after = v["after"].as_u64().unwrap_or(0);
                s.takes += 1;
                s.afters.push(after);
                s.events.retain(|(seq, _, _)| *seq > after);
                let events: Vec<Value> = s
                    .events
                    .iter()
                    .map(|(seq, body, sig)| json!({"seq": seq, "body": body, "sig": sig, "at": 0}))
                    .collect();
                Response::json(json!({"configured": s.config.is_some(), "events": events}))
            }
            _ => Response::empty(404),
        };
    }
    // The download link Meta's media lookup gives.
    if path.starts_with("/download/") {
        if req.header("authorization") != Some(&format!("Bearer {TOKEN}")) {
            return Response::empty(401);
        }
        return Response {
            status: 200,
            headers: vec![],
            body: "JPEGBYTES".into(),
        };
    }
    if req.header("authorization") != Some(&format!("Bearer {TOKEN}")) {
        st.lock().unwrap().unauthorized += 1;
        return graph_error(190);
    }
    let mut s = st.lock().unwrap();
    if path == format!("/v23.0/{PHONE_ID}/messages") {
        let body = req.json();
        let is_status = body["status"] == "read";
        if !is_status {
            if let Some(code) = s.fail_next.pop_front() {
                s.sent.push(json!({"failed": code, "body": body}));
                return graph_error(code);
            }
        }
        s.sent.push(body);
        s.next_id += 1;
        let id = format!("wamid.out{}", s.next_id);
        return Response::json(json!({
            "messaging_product": "whatsapp",
            "messages": [{ "id": id }],
        }));
    }
    if path == format!("/v23.0/{PHONE_ID}/media") {
        let body = String::from_utf8_lossy(&req.body).to_string();
        let kind = body
            .split("name=\"type\"\r\n\r\n")
            .nth(1)
            .and_then(|r| r.split("\r\n").next())
            .unwrap_or("")
            .to_string();
        s.uploads.push((kind, req.body.len()));
        return Response::json(json!({"id": "media-up-1"}));
    }
    if path == format!("/v23.0/{PHONE_ID}") {
        return Response::json(json!({
            "display_phone_number": "+1 555 0100",
            "verified_name": "Ferrule Test",
            "id": PHONE_ID,
        }));
    }
    if let Some(id) = path.strip_prefix("/v23.0/") {
        if id == "media-big" {
            return Response::json(json!({
                "url": format!("http://127.0.0.1:{port}/download/big"),
                "mime_type": "image/jpeg",
                "file_size": 999_999_999u64,
            }));
        }
        return Response::json(json!({
            "url": format!("http://127.0.0.1:{port}/download/{id}"),
            "mime_type": "image/jpeg",
            "file_size": 9,
            "id": id,
        }));
    }
    Response::empty(404)
}

/// A webhook body with one message from `from` to our number.
pub fn message(id: &str, from: &str, kind: &str, part: Value) -> Value {
    json!({
        "object": "whatsapp_business_account",
        "entry": [{ "id": "WABA", "changes": [{ "field": "messages", "value": {
            "messaging_product": "whatsapp",
            "metadata": { "display_phone_number": "15550100", "phone_number_id": PHONE_ID },
            "contacts": [{ "wa_id": from, "profile": { "name": "Max" } }],
            "messages": [{ "id": id, "from": from, "timestamp": chrono_now(), "type": kind, kind: part }],
        }}]}]
    })
}

pub fn text(id: &str, from: &str, body: &str) -> Value {
    message(id, from, "text", json!({ "body": body }))
}

/// A `failed` status for message `id` to `to`.
pub fn failed(id: &str, to: &str, code: i64) -> Value {
    json!({
        "object": "whatsapp_business_account",
        "entry": [{ "id": "WABA", "changes": [{ "field": "messages", "value": {
            "messaging_product": "whatsapp",
            "metadata": { "display_phone_number": "15550100", "phone_number_id": PHONE_ID },
            "statuses": [{ "id": id, "status": "failed", "recipient_id": to, "timestamp": "1",
                "errors": [{ "code": code, "title": "Re-engagement message" }] }],
        }}]}]
    })
}

fn chrono_now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string()
}
