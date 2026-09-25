//! What every driver shares: the HTTP client, the retry rules for a status,
//! `Retry-After`, and turning a reply into a JSON body or an error whose
//! text says `HTTP {status}` and the provider's message (M19b retries on
//! it, M21 falls back on it, M25 classes it).

use ferrule_core::error::CoreError;
use serde_json::Value;
use std::time::Duration;

/// Every driver's client: a 10-minute timeout (no streaming, and a
/// thinking model can take minutes).
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
    // `without_url()`: the URL isn't secret here, but some gateways put
    // a key in it, and these errors end up in logs and chats.
    let resp = match request.send().await.map_err(|e| e.without_url()) {
        Ok(resp) => resp,
        // No connection or no answer in time: the next try may get one.
        Err(e) if e.is_timeout() => return Err(transient(format!("request timed out: {e}"), None)),
        Err(e) if e.is_connect() => return Err(transient(format!("could not connect: {e}"), None)),
        Err(e) if e.is_request() => return Err(transient(format!("request failed: {e}"), None)),
        Err(e) => return Err(CoreError::Provider(format!("request failed: {e}"))),
    };

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
pub(crate) mod mock {
    //! A canned HTTP server on 127.0.0.1: one request in per response out,
    //! each request's raw text handed back for the test to read.
    use std::io::{Read, Write};
    use std::net::TcpListener;

    pub struct Canned {
        pub status: &'static str,
        pub headers: &'static str,
        pub body: String,
    }

    pub fn ok(body: impl Into<String>) -> Canned {
        Canned {
            status: "200 OK",
            headers: "content-type: application/json\r\n",
            body: body.into(),
        }
    }

    pub fn status(status: &'static str, headers: &'static str, body: impl Into<String>) -> Canned {
        Canned {
            status,
            headers,
            body: body.into(),
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
                let resp = format!(
                    "HTTP/1.1 {}\r\n{}content-length: {}\r\nconnection: close\r\n\r\n{}",
                    reply.status,
                    reply.headers,
                    reply.body.len(),
                    reply.body
                );
                stream.write_all(resp.as_bytes()).unwrap();
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
