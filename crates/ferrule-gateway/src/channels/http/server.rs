//! The HTTP API's listener: plain HTTP/1.1 on 127.0.0.1 (or `bind`), one request per
//! connection (`connection: close`), each on its own task so a streaming
//! answer or a long-poll doesn't hold the others up.

use super::{clients, valid_conversation, Ev, Pending, Shared, MAX_BODY};
use crate::channels::files;
use crate::error::GatewayError;
use crate::message::{Attachment, InboundMessage};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

const HEAD_MAX: usize = 16 * 1024;
/// Connections served at once; more get a 503.
const OPEN_MAX: usize = 64;
/// The longest a long-poll waits.
const WAIT_MAX: u64 = 60;
const READ_DEADLINE: Duration = Duration::from_secs(30);

pub(super) async fn run(
    shared: Arc<Shared>,
    tx: mpsc::Sender<InboundMessage>,
) -> Result<(), GatewayError> {
    let (ip, port) = (shared.cfg.bind, shared.cfg.port);
    let listener = TcpListener::bind((ip, port)).await.map_err(|e| {
        GatewayError::Channel(format!(
            "the HTTP API couldn't listen on {ip}:{port} ({e}); is another program (or ferrule instance) on it? [gateway.http] port picks another"
        ))
    })?;
    let bound = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    shared.port.store(bound, Ordering::Relaxed);
    shared.clients();
    tracing::info!("http api: listening on {ip}:{bound}");
    let tunnel = shared
        .cfg
        .tunnel
        .clone()
        .map(|bin| tokio::spawn(tunnel(shared.clone(), bin, bound)));
    let open = Arc::new(AtomicUsize::new(0));
    loop {
        tokio::select! {
            _ = tx.closed() => break,
            accepted = listener.accept() => {
                let mut sock = match accepted {
                    Ok((sock, _)) => sock,
                    Err(e) => {
                        tracing::warn!(error = %e, "http api: accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                if open.load(Ordering::Relaxed) >= OPEN_MAX {
                    tokio::spawn(async move {
                        let _ = error(&mut sock, 503, "too many connections at once").await;
                    });
                    continue;
                }
                open.fetch_add(1, Ordering::Relaxed);
                let (shared, tx, open) = (shared.clone(), tx.clone(), open.clone());
                tokio::spawn(async move {
                    handle(&shared, &tx, &mut sock).await;
                    open.fetch_sub(1, Ordering::Relaxed);
                });
            }
        }
    }
    if let Some(t) = tunnel {
        t.abort();
    }
    Ok(())
}

/// Keeps M20's quick tunnel open while the gateway runs: a new one (a new
/// address) when cloudflared dies, after a backoff.
async fn tunnel(shared: Arc<Shared>, bin: PathBuf, port: u16) {
    let mut backoff = Duration::from_secs(30);
    loop {
        match ferrule_connections::tunnel::open(&bin, port).await {
            Ok(mut t) => {
                tracing::info!(url = %t.url, "http api: public through a quick tunnel");
                *shared.tunnel_url.lock().unwrap() = Some(t.url.clone());
                shared.set_problem("tunnel", None);
                backoff = Duration::from_secs(30);
                while t.alive() {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
                *shared.tunnel_url.lock().unwrap() = None;
                shared.set_problem(
                    "tunnel",
                    Some("the quick tunnel closed; opening a new one (it gets a new address)".into()),
                );
            }
            Err(e) => shared.set_problem(
                "tunnel",
                Some(format!(
                    "the HTTP API's quick tunnel didn't open ({e:#}); it's reachable on this machine only until it does"
                )),
            ),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(300));
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

    fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("")
    }

    fn query(&self, key: &str) -> Option<String> {
        query(&self.target)
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }
}

async fn handle(shared: &Arc<Shared>, tx: &mpsc::Sender<InboundMessage>, sock: &mut TcpStream) {
    let req = match tokio::time::timeout(READ_DEADLINE, read_request(sock)).await {
        Ok(Ok(r)) => r,
        Ok(Err((status, why))) => {
            let _ = error(sock, status, why).await;
            return;
        }
        Err(_) => {
            let _ = error(sock, 408, "the request took too long to arrive").await;
            return;
        }
    };
    let key = req.header("authorization").and_then(|v| {
        let (scheme, key) = v.trim().split_once(' ')?;
        scheme.eq_ignore_ascii_case("bearer").then(|| key.trim())
    });
    let all = shared.clients();
    let Some(client) = key.and_then(|k| clients::find(&all, k)).cloned() else {
        let _ = respond(
            sock,
            401,
            &[("www-authenticate", "Bearer realm=\"ferrule\"".into())],
            &json!({ "error": "unknown or revoked key" }),
        )
        .await;
        return;
    };
    if let Some(wait) = shared.over_rate(&client.name) {
        let _ = respond(
            sock,
            429,
            &[("retry-after", wait.to_string())],
            &json!({ "error": format!("over {} requests a minute; try again in {wait} s", shared.cfg.requests_per_minute) }),
        )
        .await;
        return;
    }
    shared.seen(&client.name);
    let r = match (req.method.as_str(), req.path()) {
        ("POST", "/v1/messages") => messages(shared, tx, sock, &client.name, &req).await,
        ("GET", "/v1/events") => events(shared, sock, &client.name, &req).await,
        ("GET", p) if p.starts_with("/v1/files/") => {
            file(shared, sock, &client.name, &p["/v1/files/".len()..]).await
        }
        (_, "/v1/messages" | "/v1/events") => error(sock, 405, "wrong method").await,
        _ => {
            error(
                sock,
                404,
                "no such route: POST /v1/messages, GET /v1/events, GET /v1/files/<token>",
            )
            .await
        }
    };
    if let Err(e) = r {
        tracing::debug!(error = %e, "http api: a response didn't go out");
    }
}

#[derive(Deserialize)]
struct MessageIn {
    #[serde(default)]
    text: String,
    #[serde(default)]
    conversation: Option<String>,
    #[serde(default)]
    files: Vec<FileIn>,
}

#[derive(Deserialize)]
struct FileIn {
    name: String,
    /// Base64.
    data: String,
    #[serde(default)]
    mime: Option<String>,
}

async fn messages(
    shared: &Arc<Shared>,
    tx: &mpsc::Sender<InboundMessage>,
    sock: &mut TcpStream,
    client: &str,
    req: &Request,
) -> std::io::Result<()> {
    let body: MessageIn = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => {
            return error(
                sock,
                400,
                &format!("the body must be JSON like {{\"text\": \"…\"}} ({e})"),
            )
            .await
        }
    };
    if let Some(c) = &body.conversation {
        if !valid_conversation(c) {
            return error(
                sock,
                400,
                "a conversation is 1–64 letters, digits and . _ : -",
            )
            .await;
        }
    }
    let id = shared.next_id("r");
    let chat = match &body.conversation {
        Some(c) => format!("{client}/{c}"),
        None => client.to_string(),
    };
    let mut text = body.text;
    let mut saved = vec![];
    let mut refused = vec![];
    if !body.files.is_empty() {
        let Some(inbox) = &shared.cfg.inbox else {
            return error(sock, 400, "this gateway takes no files").await;
        };
        use base64::Engine;
        for f in &body.files {
            let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(f.data.trim()) else {
                return error(sock, 400, &format!("`{}`: data isn't base64", f.name)).await;
            };
            if bytes.len() as u64 > inbox.max_bytes() {
                refused.push(inbox.too_big(&f.name, bytes.len() as u64));
                continue;
            }
            match inbox.save("http", &id, &f.name, f.mime.as_deref(), &bytes) {
                Ok(s) => saved.push(s),
                Err(e) => refused.push(files::Refused {
                    name: f.name.clone(),
                    why: format!("it couldn't be saved ({e})"),
                }),
            }
        }
        text = files::with_notes(&text, &saved, &refused);
    }
    if text.trim().is_empty() {
        return error(sock, 400, "`text` is empty").await;
    }
    let (etx, mut erx) = mpsc::unbounded_channel();
    shared.pending.lock().unwrap().insert(
        id.clone(),
        Pending {
            chat: chat.clone(),
            conversation: body.conversation.clone(),
            parts: vec![],
            files: vec![],
            choices: vec![],
            tx: etx,
        },
    );
    let msg = InboundMessage {
        channel: "http".into(),
        chat_id: chat,
        sender: client.to_string(),
        sender_id: Some(client.to_string()),
        message_id: id.clone(),
        text,
        attachments: saved
            .iter()
            .map(|s| Attachment {
                kind: s.mime.clone(),
                url: s.path.to_string_lossy().into_owned(),
                name: Some(s.rel.clone()),
            })
            .collect(),
        reply_to: None,
        ts: clients::now(),
    };
    if tx.send(msg).await.is_err() {
        shared.pending.lock().unwrap().remove(&id);
        return error(sock, 503, "the gateway is stopping").await;
    }
    let sse = req
        .header("accept")
        .is_some_and(|a| a.contains("text/event-stream"));
    if sse {
        stream(shared, sock, &mut erx, &id, body.conversation.as_deref()).await
    } else {
        wait(shared, sock, &mut erx, &id).await
    }
}

/// A plain request: the answer as one JSON body. A client that hangs up
/// stops waiting, and the answer goes to its outbox.
async fn wait(
    shared: &Arc<Shared>,
    sock: &mut TcpStream,
    erx: &mut mpsc::UnboundedReceiver<Ev>,
    id: &str,
) -> std::io::Result<()> {
    let deadline = tokio::time::sleep(shared.timing.answer);
    tokio::pin!(deadline);
    let mut scratch = [0u8; 256];
    loop {
        tokio::select! {
            ev = erx.recv() => match ev {
                Some(Ev::Done(v)) => return respond(sock, 200, &[], &v).await,
                Some(_) => continue,
                None => return error(sock, 500, "the request was dropped").await,
            },
            n = sock.read(&mut scratch) => {
                if matches!(n, Ok(0) | Err(_)) {
                    return Ok(());
                }
            }
            _ = &mut deadline => {
                return respond(sock, 504, &[], &json!({
                    "id": id,
                    "error": "no answer yet; it will be in GET /v1/events when it comes",
                })).await;
            }
        }
    }
}

/// `Accept: text/event-stream`: `accepted`, then `delta` (the whole answer
/// so far, since an edit can change what was already said) and `message`
/// (anything else sent to the chat meanwhile, like an approval ask), then
/// `done` with the full answer.
async fn stream(
    shared: &Arc<Shared>,
    sock: &mut TcpStream,
    erx: &mut mpsc::UnboundedReceiver<Ev>,
    id: &str,
    conversation: Option<&str>,
) -> std::io::Result<()> {
    sock.write_all(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nx-accel-buffering: no\r\nconnection: close\r\n\r\n",
    )
    .await?;
    sse(
        sock,
        "accepted",
        &json!({ "id": id, "conversation": conversation }),
    )
    .await?;
    let mut tick = tokio::time::interval(shared.timing.keepalive);
    tick.tick().await;
    let mut scratch = [0u8; 256];
    loop {
        tokio::select! {
            ev = erx.recv() => match ev {
                Some(Ev::Delta(text)) => sse(sock, "delta", &json!({ "id": id, "text": text })).await?,
                Some(Ev::Message(v)) => sse(sock, "message", &v).await?,
                Some(Ev::Done(v)) => {
                    sse(sock, "done", &v).await?;
                    return sock.shutdown().await;
                }
                None => return Ok(()),
            },
            _ = tick.tick() => sock.write_all(b": keepalive\n\n").await?,
            n = sock.read(&mut scratch) => {
                if matches!(n, Ok(0) | Err(_)) {
                    return Ok(());
                }
            }
        }
    }
}

async fn sse(sock: &mut TcpStream, event: &str, data: &Value) -> std::io::Result<()> {
    let line = format!("event: {event}\ndata: {data}\n\n");
    sock.write_all(line.as_bytes()).await?;
    sock.flush().await
}

/// The outbox after `after`; with `wait`, held until something arrives.
async fn events(
    shared: &Arc<Shared>,
    sock: &mut TcpStream,
    client: &str,
    req: &Request,
) -> std::io::Result<()> {
    let mut after: u64 = req.query("after").and_then(|a| a.parse().ok()).unwrap_or(0);
    let wait: u64 = req
        .query("wait")
        .and_then(|w| w.parse().ok())
        .unwrap_or(0)
        .min(WAIT_MAX);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait);
    loop {
        let grew = shared.outbox_grew.notified();
        tokio::pin!(grew);
        grew.as_mut().enable();
        let (evs, last) = shared.events(client, after);
        if after > last {
            // The numbers started over (state.json was removed): all of it.
            after = 0;
            continue;
        }
        if !evs.is_empty() || tokio::time::Instant::now() >= deadline {
            return respond(sock, 200, &[], &json!({ "events": evs, "last": last })).await;
        }
        tokio::select! {
            _ = &mut grew => {}
            _ = tokio::time::sleep_until(deadline) => {}
        }
    }
}

/// A file the agent sent, to the client it was sent to, for an hour.
async fn file(
    shared: &Arc<Shared>,
    sock: &mut TcpStream,
    client: &str,
    token: &str,
) -> std::io::Result<()> {
    let found = {
        let files = shared.files.lock().unwrap();
        files
            .get(token)
            .filter(|f| f.client == client && f.expires > std::time::Instant::now())
            .map(|f| (f.path.clone(), f.name.clone()))
    };
    let gone = "no such file, or its link expired (links work for an hour)";
    let Some((path, name)) = found else {
        return error(sock, 404, gone).await;
    };
    let Ok(bytes) = tokio::fs::read(&path).await else {
        return error(sock, 404, gone).await;
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: {}\r\ncontent-disposition: attachment; filename=\"{}\"\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        files::mime_for(&name),
        files::safe_name(&name),
        bytes.len()
    );
    sock.write_all(head.as_bytes()).await?;
    sock.write_all(&bytes).await?;
    sock.shutdown().await
}

async fn error(sock: &mut TcpStream, status: u16, why: &str) -> std::io::Result<()> {
    respond(sock, status, &[], &json!({ "error": why })).await
}

async fn respond(
    sock: &mut TcpStream,
    status: u16,
    headers: &[(&str, String)],
    body: &Value,
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Error",
    };
    let body = body.to_string();
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    sock.write_all(head.as_bytes()).await?;
    sock.write_all(body.as_bytes()).await?;
    sock.shutdown().await
}

/// One HTTP/1.1 request, its body capped at [`MAX_BODY`].
async fn read_request(sock: &mut TcpStream) -> Result<Request, (u16, &'static str)> {
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
    if req
        .header("transfer-encoding")
        .is_some_and(|t| !t.eq_ignore_ascii_case("identity"))
    {
        return Err((411, "send a content-length, not a chunked body"));
    }
    let len: usize = match req.header("content-length") {
        Some(l) => l.parse().map_err(|_| (400, "bad content-length"))?,
        None if req.method == "POST" => return Err((411, "length required")),
        None => 0,
    };
    if len > MAX_BODY {
        return Err((413, "the body is over 64 KiB"));
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let mut chunk = [0u8; 8192];
        let n = sock
            .read(&mut chunk)
            .await
            .map_err(|_| (400, "bad request"))?;
        if n == 0 {
            return Err((400, "the body ended early"));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    req.body = body;
    Ok(req)
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
                {
                    Some(v) => {
                        out.push(v);
                        i += 2;
                    }
                    None => out.push(b'%'),
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
    fn a_query_is_decoded_to_its_last_byte() {
        assert_eq!(
            query("/v1/events?after=3&wait=25&x=a%20b%21"),
            vec![
                ("after".to_string(), "3".to_string()),
                ("wait".to_string(), "25".to_string()),
                ("x".to_string(), "a b!".to_string()),
            ]
        );
        assert_eq!(decode("100%"), "100%");
        assert_eq!(decode("%4"), "%4");
    }
}
