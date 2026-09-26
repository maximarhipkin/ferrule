//! What every driver shares: the HTTP client, the retry rules for a status,
//! `Retry-After`, and turning a reply into a JSON body or an error whose
//! text says `HTTP {status}` and the provider's message (M19b retries on
//! it, M21 falls back on it, M25 classes it).

use ferrule_core::error::CoreError;
use serde_json::Value;
use std::time::Duration;

/// Every driver's client: a 10-minute timeout on the whole call (a
/// thinking model can take minutes; a stream also has a stall limit).
pub(crate) fn client() -> reqwest::Client {
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(600));
    // Test builds only: bypass any ambient proxy (e.g. the sandbox's
    // ONECLI gateway) so tests against a local mock server don't depend
    // on NO_PROXY being set in the environment. Never affects release binaries.
    if cfg!(test) {
        builder = builder.no_proxy();
    }
    builder.build().expect("reqwest client")
}

pub(crate) fn truncate(v: &Value) -> String {
    let s = v.to_string();
    s.chars().take(500).collect()
}

pub(crate) fn transient(message: String, retry_after: Option<Duration>) -> CoreError {
    CoreError::Transient {
        message,
        retry_after,
    }
}

/// Worth another try: a rate limit, a timeout, or the server's own failure
/// (Anthropic's 529 "overloaded" included).
pub(crate) fn retryable(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

/// `Retry-After` in seconds. The HTTP-date form is rare from these APIs and
/// is ignored, falling back to backoff.
pub(crate) fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let secs: f64 = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (secs.is_finite() && secs >= 0.0).then(|| Duration::from_secs_f64(secs))
}

/// An error object in a 200 response (how OpenRouter and some gateways
/// pass on an upstream failure) that says to try again.
pub(crate) fn transient_error_object(err: &Value) -> bool {
    let code = err.get("code");
    if let Some(n) = code.and_then(Value::as_u64) {
        return n == 408 || n == 429 || (500..600).contains(&n);
    }
    let words = [code, err.get("type"), err.get("status")];
    words
        .iter()
        .filter_map(|w| w.and_then(Value::as_str))
        .any(|w| {
            let w = w.to_ascii_lowercase();
            [
                "rate_limit",
                "overloaded",
                "server_error",
                "timeout",
                "unavailable",
                "resource_exhausted",
            ]
            .iter()
            .any(|t| w.contains(t))
        })
}

/// A 2xx reply's JSON body, with its `Retry-After` for an error inside it.
pub(crate) struct Reply {
    pub body: Value,
    pub wait: Option<Duration>,
}

/// Send `request` and read the reply. A failed send, a non-2xx status and
/// a body that isn't JSON all become errors here, transient when another
/// try may pass. An error object inside a 2xx body is the caller's to read.
pub(crate) async fn send(request: reqwest::RequestBuilder) -> Result<Reply, CoreError> {
    let resp = connect(request).await?;
    read_json(resp).await
}

/// What a streaming request got back: an event stream, or (a server that
/// ignores `stream: true`) a plain JSON reply, read as [`send`] reads it.
pub(crate) enum Opened {
    Events(Events),
    Json(Reply),
}

/// [`send`] for a request that asked to stream. A non-2xx reply fails
/// exactly as it does there, `Retry-After` included.
pub(crate) async fn open(request: reqwest::RequestBuilder) -> Result<Opened, CoreError> {
    let resp = connect(request).await?;
    let is_stream = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().starts_with("text/event-stream"));
    if resp.status().is_success() && is_stream {
        Ok(Opened::Events(Events::new(resp)))
    } else {
        read_json(resp).await.map(Opened::Json)
    }
}

async fn connect(request: reqwest::RequestBuilder) -> Result<reqwest::Response, CoreError> {
    // `without_url()`: the URL isn't secret here, but some gateways put
    // a key in it, and these errors end up in logs and chats.
    match request.send().await.map_err(|e| e.without_url()) {
        Ok(resp) => Ok(resp),
        // No connection or no answer in time: the next try may get one.
        Err(e) if e.is_timeout() => Err(transient(format!("request timed out: {e}"), None)),
        Err(e) if e.is_connect() => Err(transient(format!("could not connect: {e}"), None)),
        Err(e) if e.is_request() => Err(transient(format!("request failed: {e}"), None)),
        Err(e) => Err(CoreError::Provider(format!("request failed: {e}"))),
    }
}

async fn read_json(resp: reqwest::Response) -> Result<Reply, CoreError> {
    let status = resp.status();
    let wait = retry_after(resp.headers());
    let classify = |message: String| {
        if retryable(status) {
            transient(message, wait)
        } else {
            CoreError::Provider(message)
        }
    };
    // Text first: a proxy's 502 is an HTML page, and it's still a 502.
    let text = resp.text().await.map_err(|e| {
        let e = e.without_url();
        let why = if e.is_timeout() {
            "timed out"
        } else {
            "failed"
        };
        transient(
            format!("reading the response (HTTP {status}) {why}: {e}"),
            None,
        )
    })?;
    let body: Value = match serde_json::from_str(&text) {
        Ok(body) => body,
        Err(_) => {
            let start: String = text.trim().chars().take(300).collect();
            return Err(classify(format!("HTTP {status}, not JSON: {start}")));
        }
    };
    if !status.is_success() {
        return Err(classify(format!("HTTP {status}: {}", truncate(&body))));
    }
    Ok(Reply { body, wait })
}

/// No bytes at all for this long is a dead connection, not a slow model:
/// every API here pings or sends keep-alives while it thinks.
const STALL: Duration = Duration::from_secs(300);

/// One server-sent event: its `event:` name, if any, and its `data:`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Event {
    pub name: Option<String>,
    pub data: String,
}

impl Event {
    /// The data as JSON; an event that isn't is a malformed reply.
    pub fn json(&self) -> Result<Value, CoreError> {
        serde_json::from_str(&self.data).map_err(|_| {
            let start: String = self.data.chars().take(300).collect();
            CoreError::MalformedResponse(format!("a stream event that isn't JSON: {start}"))
        })
    }
}

/// A `text/event-stream` body, read an event at a time.
pub(crate) struct Events {
    resp: reqwest::Response,
    buf: Vec<u8>,
    seen: usize,
    done: bool,
    stall: Duration,
}

impl Events {
    fn new(resp: reqwest::Response) -> Self {
        Events {
            resp,
            buf: Vec::new(),
            seen: 0,
            done: false,
            stall: STALL,
        }
    }

    /// Events read so far.
    pub fn seen(&self) -> usize {
        self.seen
    }

    /// The next event, or `None` when the body ends. A read that fails or
    /// stalls is transient: the whole call is worth another try.
    pub async fn next(&mut self) -> Result<Option<Event>, CoreError> {
        loop {
            if let Some(event) = self.take_event() {
                self.seen += 1;
                return Ok(Some(event));
            }
            if self.done {
                return Ok(None);
            }
            let chunk = match tokio::time::timeout(self.stall, self.resp.chunk()).await {
                Err(_) => {
                    return Err(transient(
                        format!(
                            "stream stalled after {} events: nothing for {}s (timed out)",
                            self.seen,
                            self.stall.as_secs()
                        ),
                        None,
                    ))
                }
                Ok(Err(e)) => {
                    return Err(transient(
                        format!(
                            "stream broke after {} events: {}",
                            self.seen,
                            e.without_url()
                        ),
                        None,
                    ))
                }
                Ok(Ok(chunk)) => chunk,
            };
            match chunk {
                Some(bytes) => self.buf.extend_from_slice(&bytes),
                None => {
                    // A last event with no blank line after it still counts.
                    self.done = true;
                    if !self.buf.is_empty() {
                        self.buf.extend_from_slice(b"\n\n");
                    }
                }
            }
        }
    }

    /// The first complete event in the buffer, skipping comments and
    /// blocks with no data.
    fn take_event(&mut self) -> Option<Event> {
        loop {
            let (end, skip) = block_end(&self.buf)?;
            let block: Vec<u8> = self.buf.drain(..end + skip).take(end).collect();
            if let Some(event) = parse_block(&String::from_utf8_lossy(&block)) {
                return Some(event);
            }
        }
    }
}

/// Where the first event block ends (a blank line, in any of the three
/// line endings) and how long the blank line is.
fn block_end(buf: &[u8]) -> Option<(usize, usize)> {
    (0..buf.len()).find_map(|i| {
        let rest = &buf[i..];
        if rest.starts_with(b"\r\n\r\n") {
            Some((i, 4))
        } else if rest.starts_with(b"\n\n") || rest.starts_with(b"\r\r") {
            Some((i, 2))
        } else {
            None
        }
    })
}

fn parse_block(block: &str) -> Option<Event> {
    let mut name = None;
    let mut data: Option<String> = None;
    for line in block.split(['\n', '\r']) {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => name = Some(value.to_string()),
            "data" => match &mut data {
                Some(d) => {
                    d.push('\n');
                    d.push_str(value);
                }
                None => data = Some(value.to_string()),
            },
            _ => {}
        }
    }
    Some(Event { name, data: data? })
}

/// A stream that ended with no terminal event.
pub(crate) fn ended_early(seen: usize) -> CoreError {
    transient(
        format!("stream ended early, after {seen} events, with no final event"),
        None,
    )
}

/// An error event inside a stream. Anthropic's types map to the status
/// the same failure has as a reply, so it classes (and retries) the same.
pub(crate) fn stream_error(err: &Value) -> CoreError {
    let kind = err.get("type").and_then(Value::as_str).unwrap_or("");
    let status = match kind {
        "overloaded_error" => Some(529),
        "rate_limit_error" => Some(429),
        "api_error" => Some(500),
        "timeout_error" => Some(408),
        _ => None,
    };
    match status {
        Some(s) => transient(format!("HTTP {s} in the stream: {}", truncate(err)), None),
        None => error_in_body(err, None),
    }
}

/// An error object in a 2xx body, as an error: transient when it says so.
pub(crate) fn error_in_body(err: &Value, wait: Option<Duration>) -> CoreError {
    let message = format!("error in an HTTP 200 response: {}", truncate(err));
    if transient_error_object(err) {
        transient(message, wait)
    } else {
        CoreError::Provider(message)
    }
}

/// The index of the last user message: what comes after it is the tool
/// loop in progress, the only part a driver replays native blocks in.
pub(crate) fn last_user(messages: &[ferrule_core::Message]) -> Option<usize> {
    messages
        .iter()
        .rposition(|m| m.role == ferrule_core::Role::User)
}

/// `msg.native`'s items when this driver may send them back verbatim: the
/// same api and model, inside the current loop, and there is something to
/// send (M23 design §2).
pub(crate) fn replayable<'m>(
    msg: &'m ferrule_core::Message,
    api: &str,
    model: &str,
    in_loop: bool,
) -> Option<&'m [Value]> {
    let native = msg.native.as_ref()?;
    (in_loop && native.api == api && native.model == model && !native.items.is_empty())
        .then_some(native.items.as_slice())
}

#[cfg(test)]
mod sse_tests {
    use super::*;

    #[test]
    fn an_event_block_reads_as_the_spec_says() {
        let e = parse_block("event: delta\ndata: {\"a\":\ndata:1}\n: a comment\nid: 7").unwrap();
        assert_eq!(e.name.as_deref(), Some("delta"));
        assert_eq!(e.data, "{\"a\":\n1}");
        assert_eq!(e.json().unwrap(), serde_json::json!({"a": 1}));
        // A comment or a keep-alive alone is no event.
        assert_eq!(parse_block(": ping"), None);
        assert_eq!(parse_block("event: ping"), None);
        assert!(matches!(
            parse_block("data: nope").unwrap().json(),
            Err(CoreError::MalformedResponse(_))
        ));
    }

    #[test]
    fn a_block_ends_at_a_blank_line_in_any_line_ending() {
        assert_eq!(block_end(b"data: 1\n\ndata: 2"), Some((7, 2)));
        assert_eq!(block_end(b"data: 1\r\n\r\n"), Some((7, 4)));
        assert_eq!(block_end(b"data: 1\r\rx"), Some((7, 2)));
        assert_eq!(block_end(b"data: 1\n"), None);
    }
}

#[cfg(test)]
pub(crate) mod mock {
    //! A canned HTTP server on 127.0.0.1: one request in per response out,
    //! each request's raw text handed back for the test to read.
    use std::io::{Read, Write};
    use std::net::TcpListener;

    pub struct Canned {
        pub status: &'static str,
        pub headers: &'static str,
        pub body: String,
        /// Written one at a time with a pause between: a stream's chunks.
        pub pieces: Vec<String>,
        /// Promise more bytes than are sent, then hang up: a stream that
        /// dies mid-way.
        pub cut: bool,
    }

    pub fn ok(body: impl Into<String>) -> Canned {
        status("200 OK", "content-type: application/json\r\n", body.into())
    }

    pub fn status(status: &'static str, headers: &'static str, body: impl Into<String>) -> Canned {
        Canned {
            status,
            headers,
            body: body.into(),
            pieces: Vec::new(),
            cut: false,
        }
    }

    /// A `text/event-stream` reply sent in these pieces.
    pub fn sse(pieces: &[&str]) -> Canned {
        Canned {
            pieces: pieces.iter().map(|p| p.to_string()).collect(),
            ..status("200 OK", "content-type: text/event-stream\r\n", "")
        }
    }

    /// [`sse`] whose connection drops after the last piece.
    pub fn sse_cut(pieces: &[&str]) -> Canned {
        Canned {
            cut: true,
            ..sse(pieces)
        }
    }

    /// One SSE event: `event:` (when named) and its JSON data.
    pub fn event(name: &str, data: &serde_json::Value) -> String {
        if name.is_empty() {
            format!("data: {data}\n\n")
        } else {
            format!("event: {name}\ndata: {data}\n\n")
        }
    }

    /// Serve `replies` in order, one connection each. Returns the base URL
    /// (ending in `/v1`) and a handle that yields every request's text.
    pub fn serve(replies: Vec<Canned>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for reply in replies {
                let (mut stream, _) = listener.accept().unwrap();
                seen.push(read_request(&mut stream));
                let len = reply.body.len()
                    + reply.pieces.iter().map(String::len).sum::<usize>()
                    + if reply.cut { 1000 } else { 0 };
                let resp = format!(
                    "HTTP/1.1 {}\r\n{}content-length: {len}\r\nconnection: close\r\n\r\n{}",
                    reply.status, reply.headers, reply.body
                );
                stream.write_all(resp.as_bytes()).unwrap();
                for piece in &reply.pieces {
                    stream.flush().unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    // The client may have stopped reading (an error event).
                    if stream.write_all(piece.as_bytes()).is_err() {
                        break;
                    }
                }
            }
            seen
        });
        (format!("http://127.0.0.1:{port}/v1"), handle)
    }

    fn read_request(stream: &mut std::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            let n = stream.read(&mut chunk).unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf);
            if let Some(end) = text.find("\r\n\r\n") {
                let len = text[..end]
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if buf.len() >= end + 4 + len {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// The JSON body of a raw request.
    pub fn body(request: &str) -> serde_json::Value {
        let at = request.find("\r\n\r\n").unwrap() + 4;
        serde_json::from_str(&request[at..]).unwrap()
    }
}
