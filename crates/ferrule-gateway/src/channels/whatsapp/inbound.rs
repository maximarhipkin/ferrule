//! What comes in from WhatsApp: Meta's webhook bodies, from the relay
//! mailbox or a local listener, checked against the app secret, then
//! turned into messages for the agent (and delivery failures into held
//! messages and a `problem`).

use super::WhatsAppChannel;
use crate::channels::access::{self, Dm};
use crate::channels::{files, hmac};
use crate::error::GatewayError;
use crate::message::{InboundMessage, OutboundMessage};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

/// Meta's webhook bodies are small; the relay caps them the same.
pub const MAX_BODY: usize = 256 * 1024;
const HEAD_MAX: usize = 16 * 1024;

/// The relay mailbox's name for a relay key: `b64url(sha256("wa:" + key))`.
/// Only someone with the key can compute it; Meta is given the URL.
pub fn mailbox_box(key: &str) -> String {
    hmac::b64url(&hmac::sha256(format!("wa:{key}").as_bytes()))
}

/// One message from a webhook body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaMessage {
    pub id: String,
    /// The sender's `wa_id` (digits): the chat.
    pub from: String,
    pub name: String,
    pub ts: i64,
    /// Meta's type: text, image, interactive, …
    pub kind: String,
    pub text: String,
    /// A file to fetch: media id, MIME type, a name for it.
    pub media: Option<(String, Option<String>, String)>,
    pub reply_to: Option<String>,
}

/// A delivery status Meta reported as `failed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failed {
    pub id: String,
    pub chat: String,
    pub code: i64,
    pub title: String,
}

/// What a webhook body says for number `phone_number_id`; everything
/// addressed to another number of the same app is left out.
pub fn parse(body: &Value, phone_number_id: &str) -> (Vec<WaMessage>, Vec<Failed>) {
    let mut messages = vec![];
    let mut failed = vec![];
    if body["object"] != "whatsapp_business_account" {
        return (messages, failed);
    }
    let changes = body["entry"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|e| e["changes"].as_array().into_iter().flatten());
    for change in changes {
        let v = &change["value"];
        if v["metadata"]["phone_number_id"].as_str() != Some(phone_number_id) {
            continue;
        }
        let name_of = |wa: &str| {
            v["contacts"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|c| c["wa_id"] == wa)
                .and_then(|c| c["profile"]["name"].as_str())
                .unwrap_or(wa)
                .to_string()
        };
        for m in v["messages"].as_array().into_iter().flatten() {
            let (Some(id), Some(from), Some(kind)) =
                (m["id"].as_str(), m["from"].as_str(), m["type"].as_str())
            else {
                continue;
            };
            let mut msg = WaMessage {
                id: id.into(),
                from: from.into(),
                name: name_of(from),
                ts: m["timestamp"]
                    .as_str()
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(0),
                kind: kind.into(),
                text: String::new(),
                media: None,
                reply_to: m["context"]["id"].as_str().map(str::to_string),
            };
            let part = &m[kind];
            match kind {
                "text" => msg.text = part["body"].as_str().unwrap_or("").into(),
                "interactive" => {
                    let reply = if part["button_reply"].is_object() {
                        &part["button_reply"]
                    } else {
                        &part["list_reply"]
                    };
                    msg.text = reply["id"].as_str().unwrap_or("").into();
                }
                // A template's quick-reply button.
                "button" => {
                    msg.text = part["payload"]
                        .as_str()
                        .or_else(|| part["text"].as_str())
                        .unwrap_or("")
                        .into()
                }
                "image" | "video" | "audio" | "document" | "sticker" => {
                    msg.text = part["caption"].as_str().unwrap_or("").into();
                    if let Some(media) = part["id"].as_str() {
                        let mime = part["mime_type"].as_str().map(str::to_string);
                        let name = part["filename"]
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| {
                                let ext = mime
                                    .as_deref()
                                    .and_then(|m| m.split('/').nth(1))
                                    .map(|e| e.split(';').next().unwrap_or(e).trim())
                                    .map(|e| match e {
                                        "jpeg" => "jpg",
                                        "mpeg" => "mp3",
                                        e => e,
                                    })
                                    .unwrap_or("bin");
                                format!("{kind}.{ext}")
                            });
                        msg.media = Some((media.into(), mime, name));
                    }
                }
                "location" => {
                    let (lat, lon) = (&part["latitude"], &part["longitude"]);
                    let place = [part["name"].as_str(), part["address"].as_str()]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join(", ");
                    msg.text = if place.is_empty() {
                        format!("[The sender shared a location: {lat}, {lon}]")
                    } else {
                        format!("[The sender shared a location: {place} ({lat}, {lon})]")
                    };
                }
                "reaction" | "system" | "ephemeral" => continue,
                _ => {}
            }
            messages.push(msg);
        }
        for s in v["statuses"].as_array().into_iter().flatten() {
            if s["status"] != "failed" {
                continue;
            }
            let err = s["errors"].as_array().and_then(|e| e.first());
            failed.push(Failed {
                id: s["id"].as_str().unwrap_or("").into(),
                chat: s["recipient_id"].as_str().unwrap_or("").into(),
                code: err.and_then(|e| e["code"].as_i64()).unwrap_or(-1),
                title: err
                    .and_then(|e| {
                        e["error_data"]["details"]
                            .as_str()
                            .or_else(|| e["title"].as_str())
                    })
                    .unwrap_or("delivery failed")
                    .into(),
            });
        }
    }
    (messages, failed)
}

impl WhatsAppChannel {
    /// A webhook body and its `X-Hub-Signature-256`: checked with the app
    /// secret, then handled. `false`: the signature is wrong.
    pub(super) async fn on_body(
        &self,
        body: &[u8],
        sig: &str,
        tx: &mpsc::Sender<InboundMessage>,
    ) -> bool {
        if !hmac::verify(self.cfg.app_secret.as_bytes(), body, sig) {
            return false;
        }
        let Ok(v) = serde_json::from_slice::<Value>(body) else {
            tracing::warn!("whatsapp: a webhook body that isn't JSON");
            return true;
        };
        let (messages, failed) = parse(&v, &self.cfg.phone_number_id);
        for f in failed {
            self.on_failed(&f.id, &f.chat, f.code, &f.title);
        }
        for m in messages {
            if !self.first_time(&m.id) {
                tracing::debug!("whatsapp: dropped a redelivered message");
                continue;
            }
            if let Some(msg) = self.on_message(m).await {
                if tx.send(msg).await.is_err() {
                    return true;
                }
            }
        }
        true
    }

    /// One message, if it's for the agent.
    async fn on_message(&self, m: WaMessage) -> Option<InboundMessage> {
        match self.access.dm(&m.from, &m.name, &m.text) {
            Dm::Admit => {}
            Dm::Paired(t) => {
                self.windows.opened(&m.from, Self::now().max(m.ts));
                self.tell(&m.from, t).await;
                return None;
            }
            // `.silent()`: a stranger is never answered.
            Dm::Tell(_) | Dm::Drop => return None,
        }
        // They wrote: the window is open, and what was held goes first.
        self.windows.opened(&m.from, Self::now().max(m.ts));
        self.flush_held(&m.from).await;
        let mut text = m.text.clone();
        let mut saved = vec![];
        let mut refused = vec![];
        if let Some((media, mime, name)) = &m.media {
            if self.cfg.inbox.is_some() {
                match self.fetch_media(media, name, &m.id).await {
                    Ok(s) => saved.push(s),
                    Err(r) => {
                        self.tell(&m.from, files::refused_reply(&r)).await;
                        refused.push(r);
                    }
                }
                text = files::with_notes(&text, &saved, &refused);
            } else {
                let kind = access::file_kind(mime.as_deref());
                if text.trim().is_empty() {
                    self.tell(&m.from, access::cannot_read_text(kind)).await;
                    return None;
                }
                text = access::unread_note(&text, kind);
            }
        } else if text.trim().is_empty() {
            let kind = match m.kind.as_str() {
                "contacts" => "contact card",
                "order" => "order",
                _ => "message",
            };
            tracing::info!(chat = %m.from, "whatsapp: got a {} message ferrule can't read", m.kind);
            self.tell(&m.from, access::cannot_read_text(kind)).await;
            return None;
        }
        if text.trim().is_empty() {
            return None;
        }
        Some(InboundMessage {
            channel: "whatsapp".into(),
            chat_id: m.from.clone(),
            sender: m.name,
            sender_id: Some(m.from),
            message_id: m.id,
            text,
            attachments: saved
                .iter()
                .map(|s| crate::message::Attachment {
                    kind: s.mime.clone(),
                    url: s.path.to_string_lossy().into_owned(),
                    name: Some(s.rel.clone()),
                })
                .collect(),
            reply_to: m.reply_to,
            ts: m.ts,
        })
    }

    /// A short answer from the adapter itself (pairing, a refused file).
    async fn tell(&self, chat: &str, text: String) {
        let msg = OutboundMessage {
            channel: "whatsapp".into(),
            chat_id: chat.to_string(),
            text,
            reply_to: None,
            attachments: vec![],
        };
        if let Err(e) = self.deliver(&msg, None).await {
            tracing::warn!(error = %e, "whatsapp: couldn't answer a message");
        }
    }

    fn set_inbound_problem(&self, p: Option<String>) {
        *self.inbound_problem.lock().unwrap() = p;
    }

    /// Relay mode: sets the mailbox up, then takes what Meta left there
    /// every few seconds. The last event taken is kept in `mailbox.json`,
    /// so a restart neither repeats nor loses one.
    pub(super) async fn run_mailbox(
        &self,
        relay: &str,
        key: &str,
        tx: mpsc::Sender<InboundMessage>,
    ) -> Result<(), GatewayError> {
        let relay = relay.trim_end_matches('/');
        let client = crate::channels::ws::http_client(
            relay,
            Duration::from_secs(10),
            Duration::from_secs(30),
        );
        let bx = mailbox_box(key);
        let base = format!("{relay}/wa/{bx}");
        let state = self.cfg.state_dir.as_ref().map(|d| d.join("mailbox.json"));
        let mut after = read_seq(state.as_ref(), &bx);
        let mut configured = false;
        let mut backoff = self.timing.backoff_min;
        loop {
            if tx.is_closed() {
                return Ok(());
            }
            if !configured {
                match self.configure(&client, &base, key).await {
                    Ok(()) => {
                        configured = true;
                        self.set_inbound_problem(None);
                    }
                    Err(e) => {
                        tracing::warn!("whatsapp: {e}");
                        self.set_inbound_problem(Some(e));
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(self.timing.backoff_max);
                        continue;
                    }
                }
            }
            match take(&client, &base, key, after).await {
                Ok((ok, events)) => {
                    *self.last_poll.lock().unwrap() = Some(SystemTime::now());
                    backoff = self.timing.backoff_min;
                    if !ok {
                        // The mailbox was wiped (a redeploy): set it up again.
                        configured = false;
                        continue;
                    }
                    self.set_inbound_problem(None);
                    for (seq, body, sig) in &events {
                        if !self.on_body(body.as_bytes(), sig, &tx).await {
                            tracing::warn!(
                                "whatsapp: the relay passed on a body whose signature doesn't match this app secret; dropped"
                            );
                        }
                        after = after.max(*seq);
                    }
                    if !events.is_empty() {
                        write_seq(state.as_ref(), &bx, after);
                        // More may be waiting: don't sleep.
                        continue;
                    }
                    tokio::time::sleep(self.timing.poll).await;
                }
                Err(e) => {
                    tracing::warn!("whatsapp: {e}");
                    self.set_inbound_problem(Some(e));
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(self.timing.backoff_max);
                }
            }
        }
    }

    /// Tells the mailbox the verify token and app secret.
    async fn configure(
        &self,
        client: &reqwest::Client,
        base: &str,
        key: &str,
    ) -> Result<(), String> {
        configure_mailbox_with(
            client,
            base,
            key,
            &self.cfg.verify_token,
            &self.cfg.app_secret,
        )
        .await
    }

    /// Listen mode: Meta's webhook, straight to 127.0.0.1:<port> through
    /// the owner's tunnel.
    pub(super) async fn run_listener(
        &self,
        port: u16,
        tx: mpsc::Sender<InboundMessage>,
    ) -> Result<(), GatewayError> {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|e| {
                let p = format!("whatsapp: couldn't listen on 127.0.0.1:{port}: {e}");
                self.set_inbound_problem(Some(p.clone()));
                GatewayError::Channel(p)
            })?;
        tracing::info!("whatsapp: listening for Meta's webhook on 127.0.0.1:{port}");
        // Requests are handled one at a time: Meta sends few, and each is
        // a quick check before the 200.
        loop {
            let (mut sock, _) = match listener.accept().await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("whatsapp: accept: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            if tx.is_closed() {
                return Ok(());
            }
            let req = match tokio::time::timeout(Duration::from_secs(10), read_request(&mut sock))
                .await
            {
                Ok(Ok(r)) => r,
                Ok(Err((status, why))) => {
                    let _ = respond(&mut sock, status, why).await;
                    continue;
                }
                Err(_) => continue,
            };
            match req.method.as_str() {
                "GET" => {
                    let q = query(&req.target);
                    let get = |k: &str| q.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
                    let ok = get("hub.mode") == Some("subscribe")
                        && get("hub.verify_token")
                            .is_some_and(|t| hmac::same(t, &self.cfg.verify_token));
                    if ok {
                        let challenge = get("hub.challenge").unwrap_or("").to_string();
                        let _ = respond(&mut sock, 200, &challenge).await;
                    } else {
                        let _ = respond(&mut sock, 403, "forbidden").await;
                    }
                }
                "POST" => {
                    let sig = req.header("x-hub-signature-256").unwrap_or("").to_string();
                    if !hmac::verify(self.cfg.app_secret.as_bytes(), &req.body, &sig) {
                        tracing::warn!("whatsapp: refused a webhook with a bad signature");
                        let _ = respond(&mut sock, 401, "bad signature").await;
                        continue;
                    }
                    // Meta wants its 200 fast; the body is handled after.
                    let _ = respond(&mut sock, 200, "ok").await;
                    drop(sock);
                    *self.last_poll.lock().unwrap() = Some(SystemTime::now());
                    self.on_body(&req.body, &sig, &tx).await;
                }
                _ => {
                    let _ = respond(&mut sock, 405, "method not allowed").await;
                }
            }
        }
    }
}

/// POST `<base>/config`.
async fn configure_mailbox_with(
    client: &reqwest::Client,
    base: &str,
    key: &str,
    verify_token: &str,
    app_secret: &str,
) -> Result<(), String> {
    let resp = client
        .post(format!("{base}/config"))
        .bearer_auth(key)
        .json(&json!({ "verify_token": verify_token, "app_secret": app_secret }))
        .send()
        .await
        .map_err(|e| {
            format!(
                "couldn't reach the relay's WhatsApp mailbox: {}",
                e.without_url()
            )
        })?;
    match resp.status().as_u16() {
        200..=299 => Ok(()),
        404 => Err("the relay has no WhatsApp mailbox: it predates M39 (run `ferrule connections relay deploy` again), or the relay key doesn't match".into()),
        401 => Err("the relay refused the relay key".into()),
        s => Err(format!("the relay's WhatsApp mailbox answered {s} when set up")),
    }
}

/// Sets the relay's mailbox up for this app: for setup and the dashboard's
/// Test, before Meta's verification request arrives.
pub async fn configure_mailbox(
    relay: &str,
    key: &str,
    verify_token: &str,
    app_secret: &str,
) -> Result<String, String> {
    let relay = relay.trim_end_matches('/');
    let client =
        crate::channels::ws::http_client(relay, Duration::from_secs(10), Duration::from_secs(30));
    let base = format!("{relay}/wa/{}", mailbox_box(key));
    configure_mailbox_with(&client, &base, key, verify_token, app_secret).await?;
    Ok(base)
}

/// POST `<base>/take`: whether the mailbox is set up, and its events.
async fn take(
    client: &reqwest::Client,
    base: &str,
    key: &str,
    after: u64,
) -> Result<(bool, Vec<(u64, String, String)>), String> {
    let resp = client
        .post(format!("{base}/take"))
        .bearer_auth(key)
        .json(&json!({ "after": after }))
        .send()
        .await
        .map_err(|e| {
            format!(
                "couldn't reach the relay's WhatsApp mailbox: {}",
                e.without_url()
            )
        })?;
    let status = resp.status();
    if !status.is_success() {
        return Err(match status.as_u16() {
            404 => "the relay has no WhatsApp mailbox: redeploy it (`ferrule connections relay deploy`)".into(),
            401 => "the relay refused the relay key".into(),
            s => format!("the relay's WhatsApp mailbox answered {s}"),
        });
    }
    let v: Value = resp.json().await.map_err(|e| {
        format!(
            "the relay's WhatsApp mailbox sent something unreadable: {}",
            e.without_url()
        )
    })?;
    let events = v["events"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| {
            Some((
                e["seq"].as_u64()?,
                e["body"].as_str()?.to_string(),
                e["sig"].as_str().unwrap_or("").to_string(),
            ))
        })
        .collect();
    Ok((v["configured"].as_bool().unwrap_or(false), events))
}

/// The last event taken from mailbox `bx` (0 for another box: a new relay
/// key starts over).
fn read_seq(path: Option<&PathBuf>, bx: &str) -> u64 {
    let v: Value = path
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    if v["box"] == bx {
        v["seq"].as_u64().unwrap_or(0)
    } else {
        0
    }
}

fn write_seq(path: Option<&PathBuf>, bx: &str, seq: u64) {
    let Some(path) = path else { return };
    let tmp = path.with_extension("json.tmp");
    let body = json!({ "box": bx, "seq": seq }).to_string();
    let r = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|_| std::fs::write(&tmp, body))
        .and_then(|_| std::fs::rename(&tmp, path));
    if let Err(e) = r {
        tracing::warn!(error = %e, "whatsapp: couldn't save mailbox.json");
    }
}

struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// One HTTP/1.1 request, its body capped at `MAX_BODY`.
async fn read_request(sock: &mut tokio::net::TcpStream) -> Result<Request, (u16, &'static str)> {
    let mut buf = Vec::with_capacity(4096);
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > HEAD_MAX {
            return Err((431, "headers too large"));
        }
        let mut chunk = [0u8; 4096];
        let n = sock
            .read(&mut chunk)
            .await
            .map_err(|_| (400, "bad request"))?;
        if n == 0 {
            return Err((400, "bad request"));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let method = first.next().unwrap_or("").to_string();
    let target = first.next().unwrap_or("/").to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_string(), v.trim().to_string()))
        .collect();
    let mut req = Request {
        method,
        target,
        headers,
        body: vec![],
    };
    if req.method != "POST" {
        return Ok(req);
    }
    let len: usize = req
        .header("content-length")
        .ok_or((411, "length required"))?
        .parse()
        .map_err(|_| (400, "bad request"))?;
    if len > MAX_BODY {
        return Err((413, "too large"));
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let mut chunk = [0u8; 8192];
        let n = sock
            .read(&mut chunk)
            .await
            .map_err(|_| (400, "bad request"))?;
        if n == 0 {
            return Err((400, "bad request"));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    req.body = body;
    Ok(req)
}

async fn respond(sock: &mut tokio::net::TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        401 => "Unauthorized",
        403 => "Forbidden",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        _ => "Bad Request",
    };
    let msg = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    sock.write_all(msg.as_bytes()).await?;
    sock.shutdown().await
}

/// A request target's query, percent-decoded.
fn query(target: &str) -> Vec<(String, String)> {
    let q = target.split_once('?').map_or("", |(_, q)| q);
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect()
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match std::str::from_utf8(&b[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                    .ok_or(())
                {
                    Ok(v) => {
                        out.push(v);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_is_decoded() {
        let q = query("/?hub.mode=subscribe&hub.verify_token=a%20b+c&hub.challenge=123%");
        assert_eq!(q[1], ("hub.verify_token".into(), "a b c".into()));
        assert_eq!(q[2].1, "123%");
    }

    #[test]
    fn the_box_depends_only_on_the_key() {
        assert_eq!(mailbox_box("k"), mailbox_box("k"));
        assert_ne!(mailbox_box("k"), mailbox_box("k2"));
        assert_eq!(mailbox_box("k").len(), 43);
    }

    #[test]
    fn only_this_numbers_messages_and_failures_are_read() {
        let body = json!({
            "object": "whatsapp_business_account",
            "entry": [{ "changes": [
                { "value": {
                    "metadata": { "phone_number_id": "111" },
                    "contacts": [{ "wa_id": "9725", "profile": { "name": "Max" } }],
                    "messages": [
                        { "id": "w1", "from": "9725", "timestamp": "1700000000", "type": "text", "text": { "body": "hi" } },
                        { "id": "w2", "from": "9725", "timestamp": "1", "type": "interactive",
                          "interactive": { "type": "button_reply", "button_reply": { "id": "/approve a1", "title": "Yes" } },
                          "context": { "id": "wOut" } },
                        { "id": "w3", "from": "9725", "timestamp": "1", "type": "image",
                          "image": { "id": "m1", "mime_type": "image/jpeg", "caption": "look" } },
                        { "id": "w4", "from": "9725", "timestamp": "1", "type": "reaction", "reaction": {} }
                    ],
                    "statuses": [
                        { "id": "o1", "status": "delivered", "recipient_id": "9725" },
                        { "id": "o2", "status": "failed", "recipient_id": "9725",
                          "errors": [{ "code": 131047, "title": "Re-engagement message" }] }
                    ]
                }},
                { "value": {
                    "metadata": { "phone_number_id": "222" },
                    "messages": [{ "id": "x", "from": "1", "type": "text", "text": { "body": "not ours" } }]
                }}
            ]}]
        });
        let (m, f) = parse(&body, "111");
        assert_eq!(m.len(), 3);
        assert_eq!(
            (m[0].name.as_str(), m[0].text.as_str(), m[0].ts),
            ("Max", "hi", 1_700_000_000)
        );
        assert_eq!(m[1].text, "/approve a1");
        assert_eq!(m[1].reply_to.as_deref(), Some("wOut"));
        assert_eq!(m[2].text, "look");
        assert_eq!(
            m[2].media,
            Some(("m1".into(), Some("image/jpeg".into()), "image.jpg".into()))
        );
        assert_eq!(f.len(), 1);
        assert_eq!((f[0].id.as_str(), f[0].code), ("o2", 131047));
        assert!(parse(&json!({"object": "page"}), "111").0.is_empty());
    }
}
